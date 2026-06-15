use std::path::PathBuf;

use fs::File;
use io::Read;

use crate::{prelude::*, types::node_data::EventSource};

pub(crate) trait DirectoryListener {
    fn is_reading(&self, event_source: EventSource) -> bool;
    fn file_mut(&mut self, event_source: EventSource) -> &mut Option<File>;
    fn on_file_creation(&mut self, new_file: PathBuf, event_source: EventSource) -> Result<()>;
    fn process_data(&mut self, data: String, event_source: EventSource) -> Result<()>;

    fn on_file_modification(&mut self, event_source: EventSource) -> Result<()> {
        let mut buf = String::new();
        let file = self.file_mut(event_source).as_mut().ok_or("No file being tracked")?;
        file.read_to_string(&mut buf)?;
        self.process_data(buf, event_source)?;
        Ok(())
    }
}
