//! Calendar providers (`CalendarEventProviding`): private iCalendar/CalDAV
//! links on both systems, Evolution Data Server on Linux, and Classic Outlook
//! on Windows. Each returns recordable meetings
//! ([`lokalbot_core::calendar::CalendarEvent`]) in a time window; the policy
//! that uses them lives in `lokalbot_core::calendar`.

#[cfg(target_os = "linux")]
mod eds;
pub mod feed;
pub mod ics;
#[cfg(any(windows, test))]
mod outlook;

use lokalbot_core::calendar::CalendarEvent;

pub use feed::{LinkRefused, normalize as normalize_link, redacted as redacted_link};
pub use ics::ParseOptions;

/// Where events come from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalendarSources {
    /// Private links (secrets from the credential store).
    pub links: Vec<String>,
    /// Linux: read Evolution Data Server too.
    pub evolution: bool,
    /// Windows: read Classic Outlook's primary calendar while open.
    pub outlook: bool,
}

impl CalendarSources {
    pub fn is_empty(&self) -> bool {
        self.links.is_empty() && !self.evolution_available() && !self.outlook_available()
    }

    fn evolution_available(&self) -> bool {
        self.evolution && cfg!(target_os = "linux")
    }

    fn outlook_available(&self) -> bool {
        self.outlook && cfg!(windows)
    }
}

/// The outcome of one refresh.
#[derive(Clone, Debug, Default)]
pub struct FetchReport {
    /// Recordable meetings from every source that answered, by start.
    pub events: Vec<CalendarEvent>,
    /// Sources that were read.
    pub sources_read: usize,
    /// One line per source that failed; links appear only redacted.
    pub problems: Vec<String>,
}

/// Reads every source. Blocking: network requests for links, D-Bus for EDS,
/// COM for Classic Outlook.
pub fn fetch(sources: &CalendarSources, options: &ParseOptions) -> FetchReport {
    let mut report = FetchReport::default();
    for link in &sources.links {
        match feed::fetch(link, options) {
            Ok(events) => {
                report.sources_read += 1;
                report.events.extend(events);
            }
            Err(err) => {
                let problem = format!("{}: {err:#}", feed::redacted(link));
                log::warn!("calendar: {problem}");
                report.problems.push(problem);
            }
        }
    }
    #[cfg(target_os = "linux")]
    if sources.evolution {
        match eds::fetch(options) {
            Ok(Some(events)) => {
                report.sources_read += 1;
                report.events.extend(events);
            }
            Ok(None) => log::debug!("calendar: Evolution Data Server is not installed"),
            Err(err) => {
                let problem = format!("Evolution Data Server: {err:#}");
                log::warn!("calendar: {problem}");
                report.problems.push(problem);
            }
        }
    }
    #[cfg(windows)]
    if sources.outlook {
        match outlook::fetch(options) {
            Ok(Some(events)) => {
                report.sources_read += 1;
                report.events.extend(events);
            }
            Ok(None) => log::debug!("calendar: Classic Outlook is not running"),
            Err(err) => {
                let problem = format!("Classic Outlook: {err:#}");
                log::warn!("calendar: {problem}");
                report.problems.push(problem);
            }
        }
    }
    report
        .events
        .sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    // The same invitation can sit in two calendars; keep one.
    let mut seen = std::collections::HashSet::new();
    report
        .events
        .retain(|event| seen.insert(event.external_id.clone()));
    report
}
