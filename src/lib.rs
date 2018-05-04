#![deny(missing_docs)]

// docs.rs is still on rust 1.26 so this has not stabilized yet
#![allow(stable_features)]
#![feature(conservative_impl_trait)]

//! A simple crate that allows child processes to be handled with mio
//! 
//! # usage
//! 
//! ``` rust
//! extern crate mio_child_process;
//! extern crate mio;
//! 
//! use mio::{Poll, Token, Ready, PollOpt, Events, Evented};
//! use std::process::{Command, Stdio};
//! use mio_child_process::{CommandAsync, ProcessEvent};
//! use std::sync::mpsc::TryRecvError;
//! 
//! fn main() {
//! let mut process = Command::new("ping");
//!    if cfg!(target_os = "linux") {
//!        process.arg("-c").arg("4");
//!    }
//!    let mut process = process.arg("8.8.8.8")
//!         .stdout(Stdio::piped())
//!         .stderr(Stdio::piped())
//!         .spawn_async()
//!         .expect("Could not spawn process");
//!     let poll = Poll::new().expect("Could not spawn poll");
//!     let mut events = Events::with_capacity(10);
//!     let token = Token(1);
//!     process.register(&poll, token, Ready::all(), PollOpt::edge()).expect("Could not register");
//!     'outer: loop {
//!         poll.poll(&mut events, None).expect("Could not poll");
//!         for event in &events {
//!             assert_eq!(event.token(), token);
//!             loop {
//!                 let result = match process.try_recv() {
//!                     Ok(r) => r,
//!                     Err(TryRecvError::Empty) => continue,
//!                     Err(TryRecvError::Disconnected) => panic!("Could not receive from process"),
//!                 };
//!                 println!("{:?}", result);
//! 
//!                 match result {
//!                     ProcessEvent::Exit(exit_status) => {
//!                         assert!(exit_status.success());
//!                         break 'outer;
//!                     },
//!                     ProcessEvent::IoError(_, _) | ProcessEvent::CommandError(_) => {
//!                         assert!(false);
//!                     },
//!                     _ => {}
//!                 }
//!             }
//!         }
//!     }
//! }
//! ```
//! 
//! # Notes
//! 
//! StdioChannel::Stdout will only be emitted when `.stdout(Stdio::piped())` is passed to the `Command`.
//! 
//! StdioChannel::Stderr will only be emitted when `.stderr(Stdio::piped())` is passed to the `Command`.
//! 
//! # Threads
//! 
//! Internally a thread gets spawned for each std stream it's listening to (stdout and stderr).
//! Another thread is started, that is in a blocking wait until the child process is done.
//! This means that mio-child-process uses between 1 to 3 threads for every process that gets started.

extern crate mio;
extern crate mio_extras;

use mio::{Evented, Poll, PollOpt, Ready, Token};
use mio_extras::channel::{channel, Receiver, Sender};
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::process::{Child, Command, ExitStatus};
use std::thread::spawn;

#[cfg(test)]
mod test;

/// Extension trait to implement an async spawner on the Command struct
pub trait CommandAsync {
    /// Spawn an async child process
    fn spawn_async(&mut self) -> Result<Process>;
}

impl CommandAsync for Command {
    fn spawn_async(&mut self) -> Result<Process> {
        let child = self.spawn()?;
        Ok(Process::from_child(child))
    }
}

/// An async child process
pub struct Process {
    receiver: Receiver<ProcessEvent>,
    stdin: Option<std::process::ChildStdin>,
}

impl Process {
    /// Try to receive an event from the child. This should be called after mio's Poll returns an event for this token
    pub fn try_recv(&mut self) -> std::result::Result<ProcessEvent, std::sync::mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Create an async child process based on  given child.
    /// 
    /// Note: this will spawn 1-3 threads in the background. These threads are blocking, but the overhead might be noticable. Use with care.
    /// 
    /// If the child is created with a Command with `stdout(Stdio::piped())`, the async process will automatically listen to the stdout stream.
    /// 
    /// If the child is created with a Command with `stderr(Stdio::piped())`, the async process will automatically listen to the stderr stream.
    pub fn from_child(mut child: Child) -> Process {
        let (sender, receiver) = channel();
        if let Some(stdout) = child.stdout.take() {
            spawn(create_reader(stdout, sender.clone(), StdioChannel::Stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn(create_reader(stderr, sender.clone(), StdioChannel::Stderr));
        }
        let stdin = child.stdin.take();
        spawn(move || {
            let result = match child.wait_with_output() {
                Err(e) => {
                    let _ = sender.send(ProcessEvent::CommandError(e));
                    return;
                }
                Ok(r) => r,
            };
            if !result.stdout.is_empty() {
                if SendResult::Abort
                    == try_send_buffer(&result.stdout, StdioChannel::Stdout, &sender)
                {
                    return;
                }
            }
            if !result.stderr.is_empty() {
                if SendResult::Abort
                    == try_send_buffer(&result.stderr, StdioChannel::Stderr, &sender)
                {
                    return;
                }
            }
            let _ = sender.send(ProcessEvent::Exit(result.status));
        });
        Process { receiver, stdin }
    }
}

impl Write for Process {
    /// Write a buffer to the Stdin stream of this child process.
    /// 
    /// If the child is not created with `.stdin(Stdio::piped())`,
    /// This function will return an error `ErrorKind::NotConnected`.
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        match self.stdin.as_mut() {
            Some(ref mut stdin) => stdin.write(buf),
            None => Err(Error::from(ErrorKind::NotConnected)),
        }
    }

    /// Flushed the Stdin stream of this child process.
    /// 
    /// If the child is not created with `.stdin(Stdio::piped())`,
    /// This function will return an error `ErrorKind::NotConnected`.
    fn flush(&mut self) -> Result<()> {
        match self.stdin.as_mut() {
            Some(ref mut stdin) => stdin.flush(),
            None => Err(Error::from(ErrorKind::NotConnected)),
        }
    }
}

impl Evented for Process {
    /// Register the child process with the poll so it can notify when the child process updates
    fn register(&self, poll: &Poll, token: Token, interest: Ready, ops: PollOpt) -> Result<()> {
        self.receiver.register(poll, token, interest, ops)
    }
    /// Reregister the child process with the poll so it can notify when the child process updates
    fn reregister(&self, poll: &Poll, token: Token, interest: Ready, ops: PollOpt) -> Result<()> {
        self.receiver.reregister(poll, token, interest, ops)
    }
    /// Deregister the child process with the poll
    fn deregister(&self, poll: &Poll) -> Result<()> {
        self.receiver.deregister(poll)
    }
}

#[derive(PartialEq, Eq, Debug)]
enum SendResult {
    Abort,
    Ok,
}

fn try_send_buffer(
    buffer: &[u8],
    channel: StdioChannel,
    sender: &Sender<ProcessEvent>,
) -> SendResult {
    let str = match std::str::from_utf8(buffer) {
        Ok(s) => s,
        Err(e) => {
            let _ = sender.send(ProcessEvent::Utf8Error(channel, e));
            return SendResult::Abort;
        }
    };
    if str.is_empty() {
        println!("Aborting try_send_buffer because we're sending empty strings");
        println!("Channel: {:?}", channel);
        return SendResult::Abort;
    }
    if let Err(_) = sender.send(ProcessEvent::Data(channel, String::from(str))) {
        SendResult::Abort
    } else {
        SendResult::Ok
    }
}

fn create_reader<T: Read + 'static>(
    mut stream: T,
    sender: Sender<ProcessEvent>,
    channel: StdioChannel,
) -> impl FnOnce() {
    move || {
        let mut buffer = [0u8; 1024];
        loop {
            match stream.read(&mut buffer[..]) {
                Ok(0) => {
                    // if we read 0 bytes from the stream, that means the stream ended
                    break;
                }
                Ok(n) => {
                    if SendResult::Abort == try_send_buffer(&buffer[..n], channel, &sender) {
                        break;
                    }
                }
                Err(e) => {
                    let _ = sender.send(ProcessEvent::IoError(channel, e));
                    break;
                }
            }
        }
    }
}

/// An event received from the child process
#[derive(Debug)]
pub enum ProcessEvent {
    /// Data is received on an StdioChannel. The String is the UTF8 interpretation of this data.
    Data(StdioChannel, String),

    /// There was an issue with starting or closing a child process.
    CommandError(std::io::Error),
    
    /// There was an issue while reading the stdout or stderr stream.
    IoError(StdioChannel, std::io::Error),

    /// There was an issue with translating the byte array from the stdout or stderr stream into UTF8
    Utf8Error(StdioChannel, std::str::Utf8Error),

    /// The process exited with the given ExitStatus.
    Exit(ExitStatus),
}

/// Describes what channel the ProcessEvent came from.
#[derive(Copy, Clone, Debug)]
pub enum StdioChannel {
    /// The Stdout stream of a child process
    Stdout,

    /// The Stdout stream of a child process
    Stderr,
}
