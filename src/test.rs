use mio::{Poll, Token, Ready, PollOpt, Events, Evented};
use std::process::{Command, Stdio};
use ::{CommandAsync, ProcessEvent};
use std::sync::mpsc::TryRecvError;

#[test]
fn test_ping() {
    let mut process = Command::new("ping")
        .arg("8.8.8.8")
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
