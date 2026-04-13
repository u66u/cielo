use std::sync::{Arc, Mutex};

#[salsa::db]
pub trait Db: salsa::Database {}

#[derive(Clone, Debug, Default)]
pub struct QueryEvent {
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryMemoryStats {
    pub query: String,
    pub entries: usize,
    pub metadata_bytes: usize,
    pub field_bytes: usize,
    pub heap_bytes: Option<usize>,
}

#[salsa::db]
#[derive(Clone)]
pub struct CieloDatabase {
    storage: salsa::Storage<Self>,
    events: Arc<Mutex<Vec<QueryEvent>>>,
}

impl Default for CieloDatabase {
    fn default() -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback_events = Arc::clone(&events);
        let callback = Box::new(move |event: salsa::Event| {
            if matches!(&event.kind, salsa::EventKind::WillExecute { .. }) {
                callback_events
                    .lock()
                    .expect("Salsa event log mutex poisoned")
                    .push(QueryEvent {
                        description: format!("{event:?}"),
                    });
            }
        });
        Self {
            storage: salsa::Storage::new(Some(callback)),
            events,
        }
    }
}

impl CieloDatabase {
    pub fn take_query_events(&self) -> Vec<QueryEvent> {
        std::mem::take(&mut *self.events.lock().expect("Salsa event log mutex poisoned"))
    }

    pub fn query_memory_stats(&self) -> Vec<QueryMemoryStats> {
        let info = (self as &dyn salsa::Database).memory_usage();
        let mut stats = info
            .queries
            .values()
            .map(|entry| QueryMemoryStats {
                query: entry.debug_name().to_owned(),
                entries: entry.count(),
                metadata_bytes: entry.size_of_metadata(),
                field_bytes: entry.size_of_fields(),
                heap_bytes: entry.heap_size_of_fields(),
            })
            .collect::<Vec<_>>();
        stats.sort_by(|lhs, rhs| lhs.query.cmp(&rhs.query));
        stats
    }
}

#[salsa::db]
impl salsa::Database for CieloDatabase {}

#[salsa::db]
impl Db for CieloDatabase {}
