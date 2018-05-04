
A simple crate that allows child processes to be handled with mio

# usage

``` rust
extern crate mio_child_process;
extern crate mio;

use mio::{Poll, Token, Ready, PollOpt, Events, Evented};
use std::process::{Command, Stdio};
use mio_child_process::{CommandAsync, ProcessEvent};
use std::sync::mpsc::TryRecvError;

fn main() {
let mut process = Command::new("ping");
   if cfg!(target_os = "linux") {
       process.arg("-c").arg("4");
   }
   let mut process = process.arg("8.8.8.8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_async()
        .expect("Could not spawn process");
    let poll = Poll::new().expect("Could not spawn poll");
    let mut events = Events::with_capacity(10);
    let token = Token(1);
    process.register(&poll, token, Ready::all(), PollOpt::edge()).expect("Could not register");
    'outer: loop {
        poll.poll(&mut events, None).expect("Could not poll");
        for event in &events {
            assert_eq!(event.token(), token);
            loop {
                let result = match process.try_recv() {
                    Ok(r) => r,
                    Err(TryRecvError::Empty) => continue,
                    Err(TryRecvError::Disconnected) => panic!("Could not receive from process"),
                };
                println!("{:?}", result);

                match result {
                    ProcessEvent::Exit(exit_status) => {
                        assert!(exit_status.success());
                        break 'outer;
                    },
                    ProcessEvent::IoError(_, _) | ProcessEvent::CommandError(_) => {
                        assert!(false);
                    },
                    _ => {}
                }
            }
        }
    }
}
```

# Notes

StdioChannel::Stdout will only be emitted when `.stdout(Stdio::piped())` is passed to the `Command`.

StdioChannel::Stderr will only be emitted when `.stderr(Stdio::piped())` is passed to the `Command`.

# Threads

Internally a thread gets spawned for each std stream it's listening to (stdout and stderr).
Another thread is started, that is in a blocking wait until the child process is done.
This means that mio-child-process uses between 1 to 3 threads for every process that gets started.
