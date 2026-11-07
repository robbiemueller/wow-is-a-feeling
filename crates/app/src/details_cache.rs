//! The On-demand half of an addon's page: fetched after a short pause on the selection,
//! remembered for the last 64 addons. Screenshots cost far more memory than text, so
//! only the most recent few addons keep their textures. Dropping a handle frees its texture.

use feeling_core::details::Details;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const MAX_ADDONS: usize = 64;
const MAX_WITH_TEXTURES: usize = 6;
pub const PAUSE: Duration = Duration::from_millis(300);
/// A failure might just have been a bad moment, so it's forgotten after this.
const RETRY_AFTER: Duration = Duration::from_secs(60);

pub enum Fetch {
    Waiting,
    Ready(Details),
    Failed { at: Instant },
}

pub struct Entry<T> {
    pub fetch: Fetch,
    pub textures: Vec<T>,
    /// Their textures were dropped to save memory, so a return visit fetches them again.
    textures_dropped: bool,
}

pub struct DetailsCache<T> {
    entries: HashMap<String, Entry<T>>,
    /// Oldest first.
    order: VecDeque<String>,
    pending: Option<(String, Instant)>,
    pause: Duration,
}

impl<T> DetailsCache<T> {
    pub fn new(pause: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            pending: None,
            pause,
        }
    }

    pub fn get(&self, id: &str) -> Option<&Entry<T>> {
        self.entries.get(id)
    }

    /// The selection moved here. Starts the pause unless the answer is already known, and
    /// hands back screenshot addresses to fetch again if their textures were dropped.
    pub fn select(&mut self, id: &str, now: Instant) -> Option<Vec<String>> {
        self.touch(id);
        match self.entries.get_mut(id) {
            Some(Entry {
                fetch: Fetch::Failed { at },
                ..
            }) if now.saturating_duration_since(*at) >= RETRY_AFTER => {
                self.pending = Some((id.to_owned(), now + self.pause));
                None
            }
            Some(entry) => {
                self.pending = None;
                let again = entry.textures_dropped;
                entry.textures_dropped = false;
                match &entry.fetch {
                    Fetch::Ready(details) if again => Some(details.images.clone()),
                    _ => None,
                }
            }
            None => {
                self.pending = Some((id.to_owned(), now + self.pause));
                None
            }
        }
    }

    /// Nothing the page wants anymore is waiting.
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// The id whose pause just ended, marked as being fetched.
    pub fn take_due(&mut self, now: Instant) -> Option<String> {
        let (id, at) = self.pending.as_ref()?;
        if now < *at {
            return None;
        }
        let id = id.clone();
        self.pending = None;
        self.touch(&id);
        self.entries.insert(
            id.clone(),
            Entry {
                fetch: Fetch::Waiting,
                textures: Vec::new(),
                textures_dropped: false,
            },
        );
        self.evict();
        Some(id)
    }

    /// When the pause ends. The only thing here that ever needs waking up for.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        self.pending
            .as_ref()
            .map(|(_, at)| at.saturating_duration_since(now))
    }

    /// Record a finished fetch, and hand back the screenshots to go and get.
    pub fn store(
        &mut self,
        id: &str,
        result: Result<Details, String>,
        now: Instant,
    ) -> Option<Vec<String>> {
        let entry = self.entries.get_mut(id)?;
        match result {
            Ok(details) => {
                let images = details.images.clone();
                entry.fetch = Fetch::Ready(details);
                (!images.is_empty()).then_some(images)
            }
            Err(text) => {
                log::info!("No details for {id}: {text}");
                entry.fetch = Fetch::Failed { at: now };
                None
            }
        }
    }

    pub fn store_texture(&mut self, id: &str, texture: T) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.textures.push(texture);
        }
        self.trim_textures();
    }

    fn touch(&mut self, id: &str) {
        if let Some(at) = self.order.iter().position(|known| known == id) {
            self.order.remove(at);
        }
        self.order.push_back(id.to_owned());
    }

    fn evict(&mut self) {
        while self.order.len() > MAX_ADDONS {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn trim_textures(&mut self) {
        let keep_from = self.order.len().saturating_sub(MAX_WITH_TEXTURES);
        for id in self.order.iter().take(keep_from) {
            if let Some(entry) = self.entries.get_mut(id)
                && !entry.textures.is_empty()
            {
                entry.textures.clear();
                entry.textures_dropped = true;
            }
        }
    }

    #[cfg(test)]
    fn cached_count(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(images: &[&str]) -> Details {
        Details {
            about: "text".to_owned(),
            changelog: None,
            images: images.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn fetched(cache: &mut DetailsCache<u32>, id: &str, now: Instant) {
        cache.select(id, now);
        assert_eq!(cache.take_due(now + PAUSE).as_deref(), Some(id));
        cache.store(id, Ok(details(&["i"])), now);
    }

    #[test]
    fn nothing_is_fetched_until_the_pause_ends() {
        let now = Instant::now();
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        cache.select("a", now);
        assert_eq!(cache.take_due(now), None);
        assert_eq!(cache.next_wake(now), Some(PAUSE));
        assert_eq!(cache.take_due(now + PAUSE).as_deref(), Some("a"));
        assert!(matches!(cache.get("a").unwrap().fetch, Fetch::Waiting));
        assert_eq!(cache.next_wake(now + PAUSE), None);
    }

    #[test]
    fn arrowing_past_a_row_fetches_nothing_for_it() {
        let now = Instant::now();
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        cache.select("a", now);
        cache.select("b", now + PAUSE / 2);
        assert_eq!(cache.take_due(now + PAUSE).as_deref(), None);
        assert_eq!(cache.take_due(now + PAUSE * 2).as_deref(), Some("b"));
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn a_known_addon_needs_no_fetch() {
        let now = Instant::now();
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        fetched(&mut cache, "a", now);
        cache.select("b", now);
        cache.select("a", now);
        assert_eq!(
            cache.next_wake(now),
            None,
            "a is ready, and b's wait is over"
        );
    }

    #[test]
    fn a_failure_is_retried_only_after_a_while() {
        let now = Instant::now();
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        cache.select("a", now);
        cache.take_due(now + PAUSE);
        cache.store("a", Err("offline".to_owned()), now);
        cache.select("a", now + Duration::from_secs(1));
        assert_eq!(cache.next_wake(now + Duration::from_secs(1)), None);
        let later = now + RETRY_AFTER;
        cache.select("a", later);
        assert_eq!(cache.take_due(later + PAUSE).as_deref(), Some("a"));
    }

    #[test]
    fn only_the_last_sixty_four_are_kept() {
        let now = Instant::now();
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        for i in 0..70 {
            fetched(&mut cache, &format!("addon{i}"), now);
        }
        assert_eq!(cache.cached_count(), MAX_ADDONS);
        assert!(cache.get("addon0").is_none());
        assert!(cache.get("addon69").is_some());
    }

    #[test]
    fn textures_are_freed_long_before_the_text() {
        let now = Instant::now();
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        fetched(&mut cache, "first", now);
        cache.store_texture("first", 1);
        for i in 0..MAX_WITH_TEXTURES {
            let id = format!("addon{i}");
            fetched(&mut cache, &id, now);
            cache.store_texture(&id, 2);
        }
        let first = cache.get("first").unwrap();
        assert!(first.textures.is_empty());
        assert!(matches!(first.fetch, Fetch::Ready(_)));
        assert_eq!(
            cache.select("first", now),
            Some(vec!["i".to_owned()]),
            "a return visit fetches them again"
        );
        assert_eq!(cache.select("first", now), None, "only once");
    }

    #[test]
    fn a_result_for_something_long_gone_is_ignored() {
        let mut cache = DetailsCache::<u32>::new(PAUSE);
        assert_eq!(cache.store("gone", Ok(details(&[])), Instant::now()), None);
        cache.store_texture("gone", 1);
        assert!(cache.get("gone").is_none());
    }
}
