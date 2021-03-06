mod request;
mod response;

use std::{
    collections::HashMap,
    convert::TryInto,
    io::{Error, ErrorKind},
    sync::Arc,
    time::Duration,
};

use clap::Parser;
use rand::{Rng, SeedableRng};
use tokio::{
    net::{TcpListener, TcpStream},
    stream::StreamExt,
    sync::RwLock,
    task,
    time::delay_for,
};

/// Contains information parsed from the command-line invocation of balancebeam. The Clap macros
/// provide a fancy way to automatically construct a command-line argument parser.
#[derive(Parser, Debug)]
#[clap(about = "Fun with load balancing")]
struct CmdOptions {
    /// IP/port to bind to
    #[clap(short, long, default_value = "0.0.0.0:1100")]
    bind: String,

    /// Upstream host to forward requests to
    #[clap(short, long)]
    upstream: Vec<String>,

    /// Perform active health checks on this interval (in seconds)
    #[clap(long, default_value = "10")]
    active_health_check_interval: usize,

    /// Path to send request to for active health checks
    #[clap(long, default_value = "/")]
    active_health_check_path: String,

    /// Maximum number of requests to accept per IP per minute (0 = unlimited)
    #[clap(long, default_value = "0")]
    max_requests_per_minute: usize,
}

#[derive(Clone)]
struct Upstream {
    address: String,
    alive: Arc<RwLock<bool>>,
}

impl Upstream {
    fn new(address: String) -> Self {
        Upstream {
            address: address,
            alive: Arc::new(RwLock::new(true)),
        }
    }

    fn new_vec(addresses: Vec<String>) -> Vec<Self> {
        let mut vec = Vec::<Upstream>::new();
        for address in addresses {
            vec.push(Upstream::new(address))
        }
        vec
    }

    fn get_addr(&self) -> &String {
        &self.address
    }

    async fn is_alive(&self) -> bool {
        *self.alive.read().await
    }

    async fn set_alive(&self, status: bool) {
        let mut alive = self.alive.write().await;
        *alive = status;
    }
}

/// Contains information about the state of balancebeam (e.g. what servers we are currently proxying
/// to, what servers have failed, rate limiting counts, etc.)
///
/// You should add fields to this struct in later milestones.
#[derive(Clone)]
struct ProxyState {
    /// How frequently we check whether upstream servers are alive (Milestone 4)
    active_health_check_interval: usize,
    /// Where we should send requests when doing active health checks (Milestone 4)
    active_health_check_path: String,
    /// Maximum number of requests an individual IP can make in a minute (Milestone 5)
    max_requests_per_minute: usize,
    /// Addresses and livenesses of servers that we are proxying to
    upstreams: Vec<Upstream>,
    /// Number of requests an individual IP has made in a minute
    rates: Arc<RwLock<HashMap<String, usize>>>,
}

#[tokio::main]
async fn main() {
    // Initialize the logging library. You can print log messages using the `log` macros:
    // https://docs.rs/log/0.4.8/log/ You are welcome to continue using print! statements; this
    // just looks a little prettier.
    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var("RUST_LOG", "debug");
    }
    pretty_env_logger::init();

    // Parse the command line arguments passed to this program
    let options = CmdOptions::parse();
    if options.upstream.len() < 1 {
        log::error!("At least one upstream server must be specified using the --upstream option.");
        std::process::exit(1);
    }

    // Start listening for connections
    let mut listener = match TcpListener::bind(&options.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Could not bind to {}: {}", options.bind, err);
            std::process::exit(1);
        }
    };
    log::info!("Listening for requests on {}", options.bind);

    // Initialize ProxyState
    let state = ProxyState {
        upstreams: Upstream::new_vec(options.upstream),
        rates: Arc::new(RwLock::new(HashMap::new())),
        active_health_check_interval: options.active_health_check_interval,
        active_health_check_path: options.active_health_check_path,
        max_requests_per_minute: options.max_requests_per_minute,
    };

    // Start active health check in seperate async task
    let state_clone = state.clone();
    task::spawn(async move { active_health_check(state_clone).await });

    // Start rate limiting with fixed window
    if state.max_requests_per_minute > 0 {
        let state_clone = state.clone();
        task::spawn(async move { reset_rates_with_fixed_window(state_clone).await });
    }

    // Spawn async task for each incoming connection
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        if let Ok(stream) = stream {
            // Handle the connection!
            let state = state.clone();
            task::spawn(async move {
                handle_connection(stream, &state).await;
            });
        }
    }
}

async fn active_health_check(state: ProxyState) {
    loop {
        delay_for(Duration::from_secs(
            state.active_health_check_interval.try_into().unwrap(),
        ))
        .await;
        for upstream in state.upstreams.iter() {
            // get TCPStream
            let mut upstream_conn = match TcpStream::connect(upstream.get_addr()).await {
                Ok(stream) => stream,
                Err(err) => {
                    log::error!(
                        "Failed to connect to upstream {}: {}",
                        upstream.get_addr(),
                        err
                    );
                    upstream.set_alive(false).await;
                    continue;
                }
            };
            // create request
            let request = http::Request::builder()
                .method(http::Method::GET)
                .uri(&state.active_health_check_path)
                .header("Host", upstream.get_addr())
                .body(Vec::new())
                .unwrap();
            // request to upstream server
            if let Err(error) = request::write_to_stream(&request, &mut upstream_conn).await {
                log::error!(
                    "Failed to send request to upstream {}: {}",
                    upstream.get_addr(),
                    error
                );
                upstream.set_alive(false).await;
                continue;
            }
            log::debug!(
                "Active health check of upstream server: {}",
                upstream.get_addr()
            );
            // check aliveness and update status
            match response::read_from_stream(&mut upstream_conn, request.method()).await {
                Ok(response) if response.status().as_u16() == 200 => {
                    log::debug!("Upstream server {} is alive", upstream.get_addr());
                    upstream.set_alive(true).await
                }
                Ok(_) => {
                    log::error!("Upstream server {} not response OK", upstream.get_addr());
                    upstream.set_alive(false).await
                }
                Err(error) => {
                    log::error!("Error reading response from server: {:?}", error);
                    upstream.set_alive(false).await
                }
            };
        }
    }
}

async fn exist_alive_upstream(upstreams: &Vec<Upstream>) -> Option<u32> {
    let mut cnt = 0;
    for upstream in upstreams.iter() {
        cnt += upstream.is_alive().await as u32;
    }
    match cnt {
        0 => None,
        cnt => Some(cnt),
    }
}

async fn connect_to_upstream(state: &ProxyState) -> Result<TcpStream, Error> {
    loop {
        match exist_alive_upstream(&state.upstreams).await {
            Some(_) => {
                let mut rng = rand::rngs::StdRng::from_entropy();
                let upstream_idx = rng.gen_range(0, state.upstreams.len());
                let upstream = &state.upstreams[upstream_idx];
                if !upstream.is_alive().await {
                    continue;
                }
                let upstream_ip = upstream.get_addr();
                match TcpStream::connect(upstream_ip).await {
                    Ok(stream) => return Ok(stream),
                    Err(err) => {
                        log::error!("Failed to connect to upstream {}: {}", upstream_ip, err);
                        upstream.set_alive(false).await;
                    }
                }
            }
            None => {
                log::error!("All upstream servers are down");
                return Err(Error::new(
                    ErrorKind::Other,
                    "All upstream servers are down",
                ));
            }
        }
    }
}

async fn send_response(client_conn: &mut TcpStream, response: &http::Response<Vec<u8>>) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!(
        "{} <- {}",
        client_ip,
        response::format_response_line(&response)
    );
    if let Err(error) = response::write_to_stream(&response, client_conn).await {
        log::warn!("Failed to send response to client: {}", error);
        return;
    }
}

async fn update_rate_and_check(state: &ProxyState, client_ip: &String) -> bool {
    let mut rates = state.rates.write().await;
    let rate = rates.get_mut(client_ip).unwrap();
    *rate += 1;
    match *rate {
        rate if rate > state.max_requests_per_minute => false,
        _ => true,
    }
}

async fn reset_rates_with_fixed_window(state: ProxyState) {
    delay_for(Duration::from_secs(60)).await;
    for rate in state.rates.write().await.values_mut() {
        *rate = 0;
    }
}

async fn handle_connection(mut client_conn: TcpStream, state: &ProxyState) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!("Connection received from {}", client_ip);

    // Add client IP to rate limit control
    if !state.rates.read().await.contains_key(&client_ip) {
        state.rates.write().await.insert(client_ip.clone(), 0);
    }

    // Open a connection to a random destination server
    let mut upstream_conn = match connect_to_upstream(state).await {
        Ok(stream) => stream,
        Err(_error) => {
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
    };
    let upstream_ip = client_conn.peer_addr().unwrap().ip().to_string();

    // The client may now send us one or more requests. Keep trying to read requests until the
    // client hangs up or we get an error.
    loop {
        // Read a request from the client
        let mut request = match request::read_from_stream(&mut client_conn).await {
            Ok(request) => request,
            // Handle case where client closed connection and is no longer sending requests
            Err(request::Error::IncompleteRequest(0)) => {
                log::debug!("Client finished sending requests. Shutting down connection");
                return;
            }
            // Handle I/O error in reading from the client
            Err(request::Error::ConnectionError(io_err)) => {
                log::info!("Error reading request from client stream: {}", io_err);
                return;
            }
            Err(error) => {
                log::debug!("Error parsing request: {:?}", error);
                let response = response::make_http_error(match error {
                    request::Error::IncompleteRequest(_)
                    | request::Error::MalformedRequest(_)
                    | request::Error::InvalidContentLength
                    | request::Error::ContentLengthMismatch => http::StatusCode::BAD_REQUEST,
                    request::Error::RequestBodyTooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
                    request::Error::ConnectionError(_) => http::StatusCode::SERVICE_UNAVAILABLE,
                });
                send_response(&mut client_conn, &response).await;
                continue;
            }
        };
        log::info!(
            "{} -> {}: {}",
            client_ip,
            upstream_ip,
            request::format_request_line(&request)
        );

        // check if client IP has reached the rate limit
        if state.max_requests_per_minute > 0 && !update_rate_and_check(state, &client_ip).await {
            let response = response::make_http_error(http::StatusCode::TOO_MANY_REQUESTS);
            send_response(&mut client_conn, &response).await;
            log::debug!("Too many response from client {}", client_ip);
            continue;
        }

        // Add X-Forwarded-For header so that the upstream server knows the client's IP address.
        // (We're the ones connecting directly to the upstream server, so without this header, the
        // upstream server will only know our IP, not the client's.)
        request::extend_header_value(&mut request, "x-forwarded-for", &client_ip);

        // Forward the request to the server
        if let Err(error) = request::write_to_stream(&request, &mut upstream_conn).await {
            log::error!(
                "Failed to send request to upstream {}: {}",
                upstream_ip,
                error
            );
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
        log::debug!("Forwarded request to server");

        // Read the server's response
        let response = match response::read_from_stream(&mut upstream_conn, request.method()).await
        {
            Ok(response) => response,
            Err(error) => {
                log::error!("Error reading response from server: {:?}", error);
                let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
                send_response(&mut client_conn, &response).await;
                return;
            }
        };
        // Forward the response to the client
        send_response(&mut client_conn, &response).await;
        log::debug!("Forwarded response to client");
    }
}
