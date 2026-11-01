use std::io;
use std::process::{Child, Command};
use std::sync::Mutex;

// Some targets set CLOEXEC after creating pipes. Protect creation without
// serializing child execution, initialization or requests.
static PROCESS_CREATION: Mutex<()> = Mutex::new(());

pub trait CommandExt {
    fn spawn_serialized(&mut self) -> io::Result<Child>;
}

impl CommandExt for Command {
    fn spawn_serialized(&mut self) -> io::Result<Child> {
        let _guard = PROCESS_CREATION.lock().unwrap();
        self.spawn()
    }
}
