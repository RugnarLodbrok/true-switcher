use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zbus::blocking::Connection;

const BUS_NAME: &str = "org.true_switcher";
const OBJECT_PATH: &str = "/org/true_switcher";

pub struct FocusService {
    _connection: Connection,
    changes: Arc<AtomicBool>,
}

impl FocusService {
    pub fn start() -> io::Result<Self> {
        let changes = Arc::new(AtomicBool::new(false));
        let connection = Connection::session().map_err(io::Error::other)?;
        connection
            .request_name(BUS_NAME)
            .map_err(io::Error::other)?;
        connection
            .object_server()
            .at(
                OBJECT_PATH,
                FocusChanged {
                    changes: changes.clone(),
                },
            )
            .map_err(io::Error::other)?;

        Ok(Self {
            _connection: connection,
            changes,
        })
    }

    pub fn changes(&self) -> Arc<AtomicBool> {
        self.changes.clone()
    }
}

struct FocusChanged {
    changes: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.true_switcher.Focus")]
impl FocusChanged {
    #[zbus(name = "FocusChanged")]
    fn focus_changed(&self) {
        self.changes.store(true, Ordering::Release);
    }
}
