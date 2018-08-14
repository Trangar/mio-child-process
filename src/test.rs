use mio::{Poll, Token, Ready, PollOpt, Events, Evented};
use std::process::{Command, Stdio};
use ::{CommandAsync, ProcessEvent};
use std::sync::mpsc::TryRecvError;

#[test]
fn test_ping() {
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

#[cfg(target_os = "windows")]
pub mod windows {
    extern crate winapi;
    use std::process::{Command, Stdio};
    use ::CommandAsync;

    #[test]
    fn test_terminate(){
        let mut process = Command::new("ping");
        let mut process = process.arg("8.8.8.8")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn_async()
            .expect("Could not spawn process");

        let pid = process.id();
        assert!(process_is_running(pid), "Expected process to run after starting it");
        process.kill().expect("Could not kill process");
        assert!(process_is_running(pid), "Expected process to be terminated after terminating it");
    }

    fn process_is_running(pid: u32) -> bool {
        let handle = unsafe {winapi::um::processthreadsapi::OpenProcess(0x0001, 1, pid) };
        !handle.is_null()
    }
}

#[cfg(target_os = "unix")]
pub mod unix {
    #[test]
    fn test_terminate(){
        unimplemented!();
    }
}
