use status_executor::{StatusExecutor, StatusSender, StdContext};

fn takes_a_while(s: StatusSender<String>) -> i32 {
    for i in 0..100 {
        s.send(format!("Currently at {}", i).to_string());
        std::thread::sleep(std::time::Duration::from_millis(13));
    }
    panic!("something went horrible :D");
}

fn main() {
    let e1 = StatusExecutor::new(StdContext::default(), takes_a_while);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let e2 = StatusExecutor::new(StdContext::default(), takes_a_while);

    while !e1.is_done() || !e2.is_done() {
        if let Some(s) = e1.status() {
            println!("E1: {}", s);
        }
        if let Some(s) = e2.status() {
            println!("E2: {}", s);
        }
    }

    println!(
        "E1: {} - {}",
        e1.latest_status().unwrap(),
        e1.take_result().unwrap()
    );
    println!(
        "E2: {} - {}",
        e2.latest_status().unwrap(),
        e2.take_result().unwrap()
    );
}
