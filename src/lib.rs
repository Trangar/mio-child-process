#![deny(missing_docs)]

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
#[cfg(target_os = "windows")]
extern crate winapi;

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
    id: u32,
}

impl Process {
    /// Try to receive an event from the child. This should be called after mio's Poll returns an event for this token
    pub fn try_recv(&mut self) -> std::result::Result<ProcessEvent, std::sync::mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    pub(crate) fn from_child(mut child: Child) -> Process {
        let (sender, receiver) = channel();
        if let Some(stdout) = child.stdout.take() {
            spawn(create_reader(stdout, sender.clone(), StdioChannel::Stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn(create_reader(stderr, sender.clone(), StdioChannel::Stderr));
        }
        let stdin = child.stdin.take();
        let id = child.id();
        spawn(move || {
            let result = match child.wait_with_output() {
                Err(e) => {
                    let _ = sender.send(ProcessEvent::CommandError(e));
                    return;
                }
                Ok(r) => r,
            };
            if !result.stdout.is_empty()
                && SendResult::Abort
                    == try_send_buffer(&result.stdout, StdioChannel::Stdout, &sender)
            {
                return;
            }
            if !result.stderr.is_empty()
                && SendResult::Abort
                    == try_send_buffer(&result.stderr, StdioChannel::Stderr, &sender)
            {
                return;
            }
            let _ = sender.send(ProcessEvent::Exit(result.status));
        });
        Process {
            receiver,
            stdin,
            id,
        }
    }

    /// Kill the process child, and all it's children.
    #[cfg(target_os = "windows")]
    pub fn kill(&mut self) -> Result<()> {
        use std::collections::HashMap;
        use std::io::Error;
        use std::mem;
        use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
        use winapi::um::processthreadsapi::{OpenProcess, TerminateProcess};
        use winapi::um::tlhelp32::{
            CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32,
            TH32CS_SNAPPROCESS,
        };
        use winapi::um::winnt::PROCESS_TERMINATE;

        // We first need to make a list of all processes and their parents
        // Then we'll go through the processes and list their ids and parent ids
        // then we'll look up the current pid, find all processes that have this pid as their parent, and kill those first
        type ParentID = u32;
        type ChildID = u32;
        let mut processes = HashMap::<ParentID, Vec<ChildID>>::new();

        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(snapshot);
            }
            return Err(Error::last_os_error());
        }

        let mut process_entry_32: PROCESSENTRY32 = unsafe { mem::zeroed() };
        process_entry_32.dwSize = mem::size_of::<PROCESSENTRY32>() as u32;
        if 0 == unsafe { Process32First(snapshot, &mut process_entry_32) } {
            unsafe {
                CloseHandle(snapshot);
            }
            return Err(Error::last_os_error());
        }

        // Push first entry
        processes
            .entry(process_entry_32.th32ParentProcessID)
            .or_insert_with(Vec::new)
            .push(process_entry_32.th32ProcessID);

        while unsafe { Process32Next(snapshot, &mut process_entry_32) } != 0 {
            // Push subsequent entries
            processes
                .entry(process_entry_32.th32ParentProcessID)
                .or_insert_with(Vec::new)
                .push(process_entry_32.th32ProcessID);
        }

        unsafe {
            CloseHandle(snapshot);
        }

        // Kill all children, then kills the process with the given `pid`
        fn kill_pid(pid: u32, processes: &HashMap<ParentID, Vec<ChildID>>) -> Result<()> {
            if let Some(children) = processes.get(&pid) {
                for child in children {
                    kill_pid(*child, processes)?;
                }
            }
            // open a handle to the given pid
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
            if handle.is_null() || 0 == unsafe { TerminateProcess(handle, 0) } {
                // handle not terminated
                Err(Error::last_os_error())
            } else {
                Ok(())
            }
        }

        kill_pid(self.id(), &processes)
    }

    /// Kill the process child, and all it's children.
    #[cfg(target_os = "linux")]
    pub fn kill(&mut self) -> Result<()> {
        extern crate libc;

        let result = unsafe { libc::kill(self.id() as i32, libc::SIGKILL) };
        if result == 0 {
            Ok(())
        } else {
            Err(Error::last_os_error())
        }
    }

    /// Returns the OS-assigned process identifier associated with this child.
    pub fn id(&self) -> u32 {
        self.id
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
    if sender
        .send(ProcessEvent::Data(channel, String::from(str)))
        .is_err()
    {
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
