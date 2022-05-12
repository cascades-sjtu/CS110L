use crate::debugger_command::DebuggerCommand;
use crate::dwarf_data::{DwarfData, Error as DwarfError};
use crate::inferior::{Inferior, Status};
use rustyline::error::ReadlineError;
use rustyline::Editor;

pub struct Debugger {
    target: String,
    history_path: String,
    readline: Editor<()>,
    inferior: Option<Inferior>,
    debug_data: DwarfData,
    breakpoints: Vec<usize>,
}

fn parse_address(address: &str) -> Option<usize> {
    let address_without_0x = if address.to_lowercase().starts_with("0x") {
        &address[2..]
    } else {
        &address[..]
    };
    usize::from_str_radix(address_without_0x, 16).ok()
}

impl Debugger {
    /// Initializes the debugger.
    pub fn new(target: &str) -> Debugger {
        // (milestone 3): initialize the DwarfData
        let debug_data = match DwarfData::from_file(target) {
            Ok(val) => val,
            Err(DwarfError::ErrorOpeningFile) => {
                println!("Could not open file {}", target);
                std::process::exit(1);
            }
            Err(DwarfError::DwarfFormatError(err)) => {
                println!("Could not debugging symbols from {}: {:?}", target, err);
                std::process::exit(1);
            }
        };

        let history_path = format!("{}/.deet_history", std::env::var("HOME").unwrap());
        let mut readline = Editor::<()>::new();
        // Attempt to load history from ~/.deet_history if it exists
        let _ = readline.load_history(&history_path);

        Debugger {
            target: target.to_string(),
            history_path,
            readline,
            inferior: None,
            debug_data,
            breakpoints: vec![],
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.get_next_command() {
                DebuggerCommand::Run(args) => {
                    if self.inferior.is_some() {
                        self.inferior.as_mut().unwrap().kill().unwrap();
                    }
                    if let Some(inferior) = Inferior::new(&self.target, &args, &self.breakpoints) {
                        // Create the inferior
                        self.inferior = Some(inferior);
                        // (milestone 1): make the inferior run
                        // You may use self.inferior.as_mut().unwrap() to get a mutable reference
                        // to the Inferior object
                        match self.inferior.as_mut().unwrap().continue_run(None).unwrap() {
                            Status::Exited(exit_code) => {
                                println!("Child exited (status {})", exit_code);
                                self.inferior = None;
                            }
                            Status::Signaled(singal) => {
                                println!("Child exited with {}", singal);
                                self.inferior = None;
                            }
                            Status::Stopped(signal, rip) => {
                                println!("Child stopped with {} at address {:#x}", signal, rip);
                                let function = self.debug_data.get_function_from_addr(rip);
                                let line = self.debug_data.get_line_from_addr(rip);
                                match (function, line) {
                                    (Some(function), Some(line)) => {
                                        println!("Stopped at {} ({})", function, line)
                                    }
                                    (_, _) => {
                                        println!("Fail to resolve stopping function and line")
                                    }
                                }
                            }
                        }
                    } else {
                        println!("Error starting subprocess");
                    }
                }
                DebuggerCommand::Cont => {
                    if self.inferior.is_none() {
                        println!("The process is not being run");
                        continue;
                    }
                    match self.inferior.as_mut().unwrap().continue_run(None).unwrap() {
                        Status::Exited(exit_code) => {
                            println!("Child exited (status {})", exit_code);
                            self.inferior = None;
                        }
                        Status::Signaled(singal) => {
                            println!("Child exited with {}", singal);
                            self.inferior = None;
                        }
                        Status::Stopped(signal, rip) => {
                            println!("Child stopped with {} at address {:#x}", signal, rip)
                        }
                    }
                }
                DebuggerCommand::Back => {
                    if self.inferior.is_none() {
                        println!("The process is not being run");
                        continue;
                    }
                    self.inferior
                        .as_mut()
                        .unwrap()
                        .print_backtrace(&self.debug_data)
                        .unwrap();
                }
                DebuggerCommand::Break(breakpoint) => {
                    let address = match breakpoint.as_str().starts_with("*") {
                        true => &breakpoint.as_str()[1..],
                        false => unimplemented!(),
                    };
                    let address_val = parse_address(address).unwrap();
                    if !self.breakpoints.contains(&address_val) {
                        self.breakpoints.push(address_val);
                    }
                    println!(
                        "Set breakpoint {} at {}",
                        self.breakpoints.iter().count(),
                        address
                    )
                }
                DebuggerCommand::Quit => {
                    if self.inferior.is_some() {
                        self.inferior.as_mut().unwrap().kill().unwrap();
                    }
                    return;
                }
            }
        }
    }

    /// This function prompts the user to enter a command, and continues re-prompting until the user
    /// enters a valid command. It uses DebuggerCommand::from_tokens to do the command parsing.
    ///
    /// You don't need to read, understand, or modify this function.
    fn get_next_command(&mut self) -> DebuggerCommand {
        loop {
            // Print prompt and get next line of user input
            match self.readline.readline("(deet) ") {
                Err(ReadlineError::Interrupted) => {
                    // User pressed ctrl+c. We're going to ignore it
                    println!("Type \"quit\" to exit");
                }
                Err(ReadlineError::Eof) => {
                    // User pressed ctrl+d, which is the equivalent of "quit" for our purposes
                    return DebuggerCommand::Quit;
                }
                Err(err) => {
                    panic!("Unexpected I/O error: {:?}", err);
                }
                Ok(line) => {
                    if line.trim().len() == 0 {
                        continue;
                    }
                    self.readline.add_history_entry(line.as_str());
                    if let Err(err) = self.readline.save_history(&self.history_path) {
                        println!(
                            "Warning: failed to save history file at {}: {}",
                            self.history_path, err
                        );
                    }
                    let tokens: Vec<&str> = line.split_whitespace().collect();
                    if let Some(cmd) = DebuggerCommand::from_tokens(&tokens) {
                        return cmd;
                    } else {
                        println!("Unrecognized command.");
                    }
                }
            }
        }
    }
}
