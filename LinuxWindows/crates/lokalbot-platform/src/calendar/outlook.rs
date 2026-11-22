use chrono::{DateTime, Utc};
use lokalbot_core::calendar::{
    CalendarEvent, RawCalendarEvent, first_meeting_url, is_recordable_meeting, provider,
    sanitize_agenda, url_host,
};
use lokalbot_core::model::CalendarParticipantIdentity;

#[cfg(windows)]
use super::ics::ParseOptions;

const OL_MEETING_CANCELED: i32 = 5;
const OL_MEETING_RECEIVED_AND_CANCELED: i32 = 7;
const OL_RESPONSE_DECLINED: i32 = 4;
const OL_BUSY_FREE: i32 = 0;
const OL_BUSY_OUT_OF_OFFICE: i32 = 3;
const OL_RECIPIENT_RESOURCE: i32 = 3;

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub(crate) struct RawOutlookAttendee {
    pub name: Option<String>,
    pub email: Option<String>,
    pub recipient_type: i32,
    pub response_status: i32,
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) struct RawOutlookItem {
    pub uid: String,
    pub subject: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub is_all_day: bool,
    pub busy_status: i32,
    pub meeting_status: i32,
    pub response_status: i32,
    pub location: Option<String>,
    pub body: Option<String>,
    pub online_meeting_external_link: Option<String>,
    pub skype_teams_meeting_url: Option<String>,
    pub online_meeting_conf_link: Option<String>,
    pub attendees: Option<Vec<RawOutlookAttendee>>,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn validated_https_meeting_url(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = trimmed
        .as_bytes()
        .windows(8)
        .position(|w| w.eq_ignore_ascii_case(b"https://"))?;
    let tail = &trimmed[start..];
    let end = tail
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`'))
        .unwrap_or(tail.len());
    let candidate = tail[..end].trim_end_matches(['.', ',', ';', ')', ']', '}']);
    let parsed = url::Url::parse(candidate).ok()?;
    if parsed.scheme() != "https" || url_host(candidate).is_none() {
        return None;
    }
    Some(candidate.to_string())
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clean_smtp_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('/') {
        return None;
    }
    let normalized = CalendarParticipantIdentity::normalized_email_address(trimmed)?;
    let (_, domain) = normalized.split_once('@')?;
    domain.contains('.').then_some(normalized)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn map_item(
    item: RawOutlookItem,
    calendar_title: Option<&str>,
    my_addresses: &[String],
) -> Option<CalendarEvent> {
    let end = item.end.max(item.start);
    let meeting_url = validated_https_meeting_url(item.online_meeting_external_link.as_deref())
        .or_else(|| validated_https_meeting_url(item.skype_teams_meeting_url.as_deref()))
        .or_else(|| validated_https_meeting_url(item.online_meeting_conf_link.as_deref()))
        .or_else(|| first_meeting_url(item.location.as_deref()))
        .or_else(|| first_meeting_url(item.body.as_deref()));
    let has_non_http_conf_link = item
        .online_meeting_conf_link
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    let has_meeting_url = meeting_url.is_some() || has_non_http_conf_link;

    let declined_by_me = item.response_status == OL_RESPONSE_DECLINED
        || item
            .attendees
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|a| {
                a.response_status == OL_RESPONSE_DECLINED
                    && a.email
                        .as_ref()
                        .is_some_and(|email| my_addresses.contains(email))
            });
    let has_attendees = match &item.attendees {
        Some(list) => !list.is_empty(),
        None => matches!(item.meeting_status, 1 | 3),
    };
    let title = item
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);

    let raw = RawCalendarEvent {
        title: title.clone(),
        is_all_day: item.is_all_day,
        is_canceled: matches!(
            item.meeting_status,
            OL_MEETING_CANCELED | OL_MEETING_RECEIVED_AND_CANCELED
        ),
        is_declined_by_me: declined_by_me,
        availability_free: item.busy_status == OL_BUSY_FREE,
        availability_unavailable: item.busy_status == OL_BUSY_OUT_OF_OFFICE,
        has_attendees,
        has_meeting_url,
    };
    if !is_recordable_meeting(&raw) {
        return None;
    }

    let identities: Vec<CalendarParticipantIdentity> = item
        .attendees
        .unwrap_or_default()
        .into_iter()
        .filter(|a| {
            a.recipient_type != OL_RECIPIENT_RESOURCE && a.response_status != OL_RESPONSE_DECLINED
        })
        .filter(|a| {
            a.email
                .as_ref()
                .is_none_or(|email| !my_addresses.contains(email))
        })
        .filter_map(|a| CalendarParticipantIdentity::new("", a.name.as_deref(), a.email.as_deref()))
        .collect();
    let identities = CalendarParticipantIdentity::normalized(&identities);

    Some(CalendarEvent {
        provider: provider::OUTLOOK.into(),
        external_id: CalendarEvent::occurrence_id(&item.uid, item.start),
        title: title?,
        start: item.start,
        end,
        meeting_url,
        source_calendar_title: calendar_title.map(str::to_string),
        participant_names: identities.iter().filter_map(|i| i.name.clone()).collect(),
        participant_identities: identities,
        agenda: sanitize_agenda(item.body.as_deref()),
    })
}

#[cfg(windows)]
mod com {
    use anyhow::{Context as _, Result, anyhow};
    use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone as _, Utc};
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::Globalization::{
        DATE_SHORTDATE, GetDateFormatEx, GetTimeFormatEx, LOCALE_USER_DEFAULT, TIME_NOSECONDS,
    };
    use windows::Win32::System::Com::{
        CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, DISPATCH_FLAGS,
        DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPATCH_PROPERTYPUT, DISPPARAMS, IDispatch,
    };
    use windows::Win32::System::Ole::{DISPID_PROPERTYPUT, GetActiveObject};
    use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_DATE, VariantTimeToSystemTime};
    use windows::core::{GUID, IUnknown, Interface, PCWSTR, w};

    use super::*;

    const OL_FOLDER_CALENDAR: i32 = 9;
    const OL_APPOINTMENT_CLASS: i32 = 26;
    const MAX_ITEMS: usize = 1_024;
    const MAX_ACCOUNTS: i32 = 32;
    const MAX_RECIPIENTS: i32 = 200;

    const PR_SMTP_ADDRESS_W: &str = "http://schemas.microsoft.com/mapi/proptag/0x39FE001F";
    const PROP_ONLINE_MEETING_EXTERNAL_LINK: &str = "http://schemas.microsoft.com/mapi/string/{00020329-0000-0000-C000-000000000046}/OnlineMeetingExternalLink";
    const PROP_SKYPE_TEAMS_MEETING_URL: &str = "http://schemas.microsoft.com/mapi/string/{00020329-0000-0000-C000-000000000046}/SkypeTeamsMeetingUrl";
    const PROP_ONLINE_MEETING_CONF_LINK: &str = "http://schemas.microsoft.com/mapi/string/{00020329-0000-0000-C000-000000000046}/OnlineMeetingConfLink";

    struct ComScope {
        initialized: bool,
    }

    impl ComScope {
        fn enter() -> Self {
            let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            Self {
                initialized: result.is_ok(),
            }
        }
    }

    impl Drop for ComScope {
        fn drop(&mut self) {
            if self.initialized {
                unsafe { CoUninitialize() };
            }
        }
    }

    pub fn fetch(options: &ParseOptions) -> Result<Option<Vec<CalendarEvent>>> {
        let _com = ComScope::enter();
        let Ok(clsid) = (unsafe { CLSIDFromProgID(w!("Outlook.Application")) }) else {
            return Ok(None);
        };
        let mut unknown: Option<IUnknown> = None;
        if unsafe { GetActiveObject(&clsid, None, &mut unknown) }.is_err() {
            return Ok(None);
        }
        let Some(unknown) = unknown else {
            return Ok(None);
        };
        let app: IDispatch = unknown.cast().context("Outlook did not expose IDispatch")?;
        let session = dispatch_call(&app, "GetNamespace", &mut [VARIANT::from("MAPI")])
            .and_then(|v| variant_dispatch(&v))
            .context("could not open MAPI session")?;
        let Ok(folder_var) = dispatch_call(
            &session,
            "GetDefaultFolder",
            &mut [VARIANT::from(OL_FOLDER_CALENDAR)],
        ) else {
            return Ok(None);
        };
        let folder = variant_dispatch(&folder_var).context("default calendar folder missing")?;
        let calendar_title = dispatch_get_string(&folder, "Name")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut my_addresses = options.my_addresses.clone();
        if let Some(primary) = primary_account_smtp(&session, &folder)
            && !my_addresses.contains(&primary)
        {
            my_addresses.push(primary);
        }

        let items = dispatch_get_dispatch(&folder, "Items")
            .context("could not open Outlook calendar items")?;
        dispatch_call(&items, "Sort", &mut [VARIANT::from("[Start]")])
            .context("could not sort Outlook calendar items")?;
        dispatch_put(&items, "IncludeRecurrences", VARIANT::from(true))
            .context("could not enable Outlook recurrence expansion")?;

        let from_local = (options.from - Duration::minutes(1)).with_timezone(&Local);
        let to_local = (options.to + Duration::minutes(1)).with_timezone(&Local);
        let filter = format!(
            "[Start] < '{}' AND [End] > '{}'",
            jet_datetime(to_local)?,
            jet_datetime(from_local)?,
        );
        let restricted = dispatch_call(&items, "Restrict", &mut [VARIANT::from(filter.as_str())])
            .and_then(|v| variant_dispatch(&v))
            .context("could not filter Outlook calendar items")?;

        let mut events = Vec::new();
        let mut current = dispatch_call(&restricted, "GetFirst", &mut [])
            .ok()
            .and_then(|v| variant_dispatch(&v).ok());

        for _ in 0..MAX_ITEMS {
            let Some(item) = current else {
                break;
            };
            if dispatch_get_i32(&item, "Class").ok() == Some(OL_APPOINTMENT_CLASS)
                && let Some((start, end)) = item_bounds(&item)
            {
                if start >= options.to {
                    break;
                }
                if end > options.from
                    && let Some(raw) = read_appointment(&item, start, end)
                    && let Some(event) = map_item(raw, calendar_title.as_deref(), &my_addresses)
                {
                    events.push(event);
                }
            }
            current = dispatch_call(&restricted, "GetNext", &mut [])
                .ok()
                .and_then(|v| variant_dispatch(&v).ok());
        }

        events.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        events.dedup_by(|a, b| a.external_id == b.external_id);
        Ok(Some(events))
    }

    fn primary_account_smtp(session: &IDispatch, folder: &IDispatch) -> Option<String> {
        let folder_store_id = dispatch_get_string(folder, "StoreID").ok();
        let accounts = dispatch_get_dispatch(session, "Accounts").ok()?;
        let count = dispatch_get_i32(&accounts, "Count")
            .ok()?
            .clamp(0, MAX_ACCOUNTS);
        let mut first_smtp = None;
        for index in 1..=count {
            let Ok(account_var) = dispatch_call(&accounts, "Item", &mut [VARIANT::from(index)])
            else {
                continue;
            };
            let Ok(account) = variant_dispatch(&account_var) else {
                continue;
            };
            let smtp = dispatch_get_string(&account, "SmtpAddress")
                .ok()
                .and_then(|s| clean_smtp_address(&s));
            if first_smtp.is_none() {
                first_smtp = smtp.clone();
            }
            if let Some(target_store) = folder_store_id.as_deref()
                && let Ok(delivery) = dispatch_get_dispatch(&account, "DeliveryStore")
                && let Ok(store_id) = dispatch_get_string(&delivery, "StoreID")
                && store_id == target_store
                && smtp.is_some()
            {
                return smtp;
            }
        }
        first_smtp
    }

    fn item_bounds(item: &IDispatch) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let start = dispatch_get_utc_date(item, "StartUTC").ok()?;
        let end = dispatch_get_utc_date(item, "EndUTC").ok()?;
        Some((start, end.max(start)))
    }

    fn read_appointment(
        item: &IDispatch,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<RawOutlookItem> {
        let uid = dispatch_get_string(item, "GlobalAppointmentID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                dispatch_get_string(item, "EntryID")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })?;
        let meeting_status = dispatch_get_i32(item, "MeetingStatus").unwrap_or(0);
        let props = dispatch_get_dispatch(item, "PropertyAccessor").ok();
        let mapi_string = |schema: &str| -> Option<String> {
            let props = props.as_ref()?;
            dispatch_call(props, "GetProperty", &mut [VARIANT::from(schema)])
                .ok()
                .and_then(|v| variant_string(&v).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let attendees = if meeting_status == 0 {
            Some(Vec::new())
        } else {
            read_recipients(item)
        };
        Some(RawOutlookItem {
            uid,
            subject: dispatch_get_string(item, "Subject").ok(),
            start,
            end,
            is_all_day: dispatch_get_bool(item, "AllDayEvent").unwrap_or(false),
            busy_status: dispatch_get_i32(item, "BusyStatus").unwrap_or(2),
            meeting_status,
            response_status: dispatch_get_i32(item, "ResponseStatus").unwrap_or(0),
            location: dispatch_get_string(item, "Location").ok(),
            body: dispatch_get_string(item, "Body").ok(),
            online_meeting_external_link: mapi_string(PROP_ONLINE_MEETING_EXTERNAL_LINK),
            skype_teams_meeting_url: mapi_string(PROP_SKYPE_TEAMS_MEETING_URL),
            online_meeting_conf_link: mapi_string(PROP_ONLINE_MEETING_CONF_LINK),
            attendees,
        })
    }

    fn read_recipients(item: &IDispatch) -> Option<Vec<RawOutlookAttendee>> {
        let recipients = dispatch_get_dispatch(item, "Recipients").ok()?;
        let count = dispatch_get_i32(&recipients, "Count")
            .ok()?
            .clamp(0, MAX_RECIPIENTS);
        let mut list = Vec::with_capacity(count as usize);
        for index in 1..=count {
            let Ok(var) = dispatch_call(&recipients, "Item", &mut [VARIANT::from(index)]) else {
                continue;
            };
            let Ok(recipient) = variant_dispatch(&var) else {
                continue;
            };
            let smtp = dispatch_get_dispatch(&recipient, "PropertyAccessor")
                .ok()
                .and_then(|pa| {
                    dispatch_call(&pa, "GetProperty", &mut [VARIANT::from(PR_SMTP_ADDRESS_W)]).ok()
                })
                .and_then(|v| variant_string(&v).ok())
                .and_then(|s| clean_smtp_address(&s))
                .or_else(|| {
                    dispatch_get_string(&recipient, "Address")
                        .ok()
                        .and_then(|s| clean_smtp_address(&s))
                });
            list.push(RawOutlookAttendee {
                name: dispatch_get_string(&recipient, "Name").ok(),
                email: smtp,
                recipient_type: dispatch_get_i32(&recipient, "Type").unwrap_or(1),
                response_status: dispatch_get_i32(&recipient, "MeetingResponseStatus").unwrap_or(0),
            });
        }
        Some(list)
    }

    fn jet_datetime(at: DateTime<Local>) -> Result<String> {
        use chrono::{Datelike as _, Timelike as _};
        let st = SYSTEMTIME {
            wYear: at.year() as u16,
            wMonth: at.month() as u16,
            wDay: at.day() as u16,
            wHour: at.hour() as u16,
            wMinute: at.minute() as u16,
            wSecond: 0,
            wDayOfWeek: 0,
            wMilliseconds: 0,
        };
        let mut date_buf = [0u16; 64];
        let date_len = unsafe {
            GetDateFormatEx(
                PCWSTR::null(),
                DATE_SHORTDATE,
                Some(&st),
                PCWSTR::null(),
                Some(&mut date_buf),
                PCWSTR::null(),
            )
        };
        if date_len <= 1 {
            return Err(anyhow!("GetDateFormatEx failed"));
        }
        let mut time_buf = [0u16; 64];
        let time_len = unsafe {
            GetTimeFormatEx(
                PCWSTR::null(),
                TIME_NOSECONDS,
                Some(&st),
                PCWSTR::null(),
                Some(&mut time_buf),
            )
        };
        if time_len <= 1 {
            return Err(anyhow!("GetTimeFormatEx failed"));
        }
        let date = String::from_utf16_lossy(&date_buf[..(date_len as usize - 1)]);
        let time = String::from_utf16_lossy(&time_buf[..(time_len as usize - 1)]);
        Ok(format!("{date} {time}"))
    }

    fn dispid(dispatch: &IDispatch, name: &str) -> Result<i32> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let pcwstr = PCWSTR(wide.as_ptr());
        let mut id = 0i32;
        unsafe {
            dispatch.GetIDsOfNames(&GUID::zeroed(), &pcwstr, 1, LOCALE_USER_DEFAULT, &mut id)
        }
        .with_context(|| format!("Outlook property/method not found: {name}"))?;
        Ok(id)
    }

    fn invoke(
        dispatch: &IDispatch,
        name: &str,
        flags: DISPATCH_FLAGS,
        args: &mut [VARIANT],
    ) -> Result<VARIANT> {
        let id = dispid(dispatch, name)?;
        let mut dispid_named = DISPID_PROPERTYPUT;
        let params = if flags == DISPATCH_PROPERTYPUT {
            DISPPARAMS {
                rgvarg: args.as_mut_ptr(),
                rgdispidNamedArgs: &mut dispid_named,
                cArgs: args.len() as u32,
                cNamedArgs: 1,
            }
        } else {
            DISPPARAMS {
                rgvarg: if args.is_empty() {
                    std::ptr::null_mut()
                } else {
                    args.as_mut_ptr()
                },
                rgdispidNamedArgs: std::ptr::null_mut(),
                cArgs: args.len() as u32,
                cNamedArgs: 0,
            }
        };
        let mut result = VARIANT::default();
        unsafe {
            dispatch.Invoke(
                id,
                &GUID::zeroed(),
                LOCALE_USER_DEFAULT,
                flags,
                &params,
                Some(&mut result),
                None,
                None,
            )
        }
        .with_context(|| format!("Outlook COM call failed: {name}"))?;
        Ok(result)
    }

    fn dispatch_call(dispatch: &IDispatch, method: &str, args: &mut [VARIANT]) -> Result<VARIANT> {
        invoke(dispatch, method, DISPATCH_METHOD, args)
    }

    fn dispatch_get(dispatch: &IDispatch, prop: &str) -> Result<VARIANT> {
        invoke(dispatch, prop, DISPATCH_PROPERTYGET, &mut [])
    }

    fn dispatch_put(dispatch: &IDispatch, prop: &str, value: VARIANT) -> Result<()> {
        let mut args = [value];
        let _ = invoke(dispatch, prop, DISPATCH_PROPERTYPUT, &mut args)?;
        Ok(())
    }

    fn dispatch_get_dispatch(dispatch: &IDispatch, prop: &str) -> Result<IDispatch> {
        variant_dispatch(&dispatch_get(dispatch, prop)?)
    }

    fn dispatch_get_string(dispatch: &IDispatch, prop: &str) -> Result<String> {
        variant_string(&dispatch_get(dispatch, prop)?)
    }

    fn dispatch_get_i32(dispatch: &IDispatch, prop: &str) -> Result<i32> {
        let v = dispatch_get(dispatch, prop)?;
        i32::try_from(&v).context("expected integer from Outlook")
    }

    fn dispatch_get_bool(dispatch: &IDispatch, prop: &str) -> Result<bool> {
        let v = dispatch_get(dispatch, prop)?;
        bool::try_from(&v).context("expected boolean from Outlook")
    }

    fn dispatch_get_utc_date(dispatch: &IDispatch, prop: &str) -> Result<DateTime<Utc>> {
        let v = dispatch_get(dispatch, prop)?;
        if v.vt() != VT_DATE {
            return Err(anyhow!("expected VT_DATE from Outlook"));
        }
        let vdate = unsafe { v.Anonymous.Anonymous.Anonymous.date };
        let mut st = SYSTEMTIME::default();
        if unsafe { VariantTimeToSystemTime(vdate, &mut st) } == 0 {
            return Err(anyhow!("invalid Outlook date"));
        }
        let date = NaiveDate::from_ymd_opt(st.wYear.into(), st.wMonth.into(), st.wDay.into())
            .and_then(|d| d.and_hms_opt(st.wHour.into(), st.wMinute.into(), st.wSecond.into()))
            .context("Outlook date out of range")?;
        Ok(Utc.from_utc_datetime(&date))
    }

    fn variant_dispatch(v: &VARIANT) -> Result<IDispatch> {
        IDispatch::try_from(v).context("expected IDispatch from Outlook")
    }

    fn variant_string(v: &VARIANT) -> Result<String> {
        if v.vt() == VT_BSTR {
            return Ok(unsafe { (*v.Anonymous.Anonymous.Anonymous.bstrVal).to_string() });
        }
        Err(anyhow!("expected string from Outlook"))
    }
}

#[cfg(windows)]
pub use com::fetch;

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    fn sample_item() -> RawOutlookItem {
        RawOutlookItem {
            uid: "040000008200E00074C5B7101A82E008".into(),
            subject: Some("Weekly sync".into()),
            start: at(1_700_000_000),
            end: at(1_700_003_600),
            is_all_day: false,
            busy_status: 2,
            meeting_status: 1,
            response_status: 3,
            location: None,
            body: Some("Agenda:\nReview blockers".into()),
            online_meeting_external_link: Some("https://meet.corp.local/alice/ABC12345".into()),
            skype_teams_meeting_url: None,
            online_meeting_conf_link: None,
            attendees: Some(vec![
                RawOutlookAttendee {
                    name: Some("Me".into()),
                    email: Some("me@corp.local".into()),
                    recipient_type: 1,
                    response_status: 3,
                },
                RawOutlookAttendee {
                    name: Some("Ana Petrović".into()),
                    email: Some("ana@corp.local".into()),
                    recipient_type: 1,
                    response_status: 3,
                },
                RawOutlookAttendee {
                    name: Some("Conf Room 1".into()),
                    email: Some("room1@corp.local".into()),
                    recipient_type: OL_RECIPIENT_RESOURCE,
                    response_status: 3,
                },
                RawOutlookAttendee {
                    name: Some("Declined Guest".into()),
                    email: Some("declined@corp.local".into()),
                    recipient_type: 2,
                    response_status: OL_RESPONSE_DECLINED,
                },
            ]),
        }
    }

    #[test]
    fn maps_corporate_skype_for_business_meeting_and_filters_attendees() {
        let event = map_item(sample_item(), Some("Calendar"), &["me@corp.local".into()]).unwrap();
        assert_eq!(event.provider, "outlook");
        assert_eq!(
            event.external_id,
            "040000008200E00074C5B7101A82E008#1700000000"
        );
        assert_eq!(event.title, "Weekly sync");
        assert_eq!(
            event.meeting_url.as_deref(),
            Some("https://meet.corp.local/alice/ABC12345")
        );
        assert_eq!(event.source_calendar_title.as_deref(), Some("Calendar"));
        assert_eq!(event.participant_names, ["Ana Petrović"]);
        assert_eq!(
            event.participant_identities[0].email_address.as_deref(),
            Some("ana@corp.local")
        );
        assert_eq!(event.agenda.as_deref(), Some("Agenda:\nReview blockers"));
    }

    #[test]
    fn sip_conf_link_marks_meeting_recordable_without_exposing_sip_as_http_url() {
        let mut item = sample_item();
        item.online_meeting_external_link = None;
        item.online_meeting_conf_link =
            Some("sip:alice@corp.local;gruu;opaque=app:conf:focus:id:1A2B3C4D".into());
        item.attendees = Some(Vec::new());
        item.busy_status = OL_BUSY_FREE;
        let event = map_item(item, None, &[]).unwrap();
        assert_eq!(event.meeting_url, None);
    }

    #[test]
    fn object_model_guard_fallback_preserves_meeting_when_recipients_unavailable() {
        let mut item = sample_item();
        item.online_meeting_external_link = None;
        item.attendees = None;
        item.body = None;
        item.meeting_status = 3;
        item.busy_status = OL_BUSY_FREE;
        let event = map_item(item, None, &[]).unwrap();
        assert!(event.participant_identities.is_empty());
        assert_eq!(event.agenda, None);
    }

    #[test]
    fn rejects_all_day_canceled_declined_ooo_and_free_focus_blocks() {
        let mut all_day = sample_item();
        all_day.is_all_day = true;
        assert!(map_item(all_day, None, &[]).is_none());

        let mut canceled = sample_item();
        canceled.meeting_status = OL_MEETING_CANCELED;
        assert!(map_item(canceled, None, &[]).is_none());

        let mut declined = sample_item();
        declined.response_status = OL_RESPONSE_DECLINED;
        assert!(map_item(declined, None, &[]).is_none());

        let mut ooo = sample_item();
        ooo.busy_status = OL_BUSY_OUT_OF_OFFICE;
        assert!(map_item(ooo, None, &[]).is_none());

        let mut focus = sample_item();
        focus.busy_status = OL_BUSY_FREE;
        focus.meeting_status = 0;
        focus.online_meeting_external_link = None;
        focus.attendees = Some(Vec::new());
        assert!(map_item(focus, None, &[]).is_none());
    }

    #[test]
    fn cleans_smtp_and_rejects_exchange_x500_addresses() {
        assert_eq!(
            clean_smtp_address("  Ana.Petrovic@Corp.Local "),
            Some("ana.petrovic@corp.local".into())
        );
        assert_eq!(
            clean_smtp_address("/o=ExchangeLabs/ou=Exchange Administrative Group/cn=ana"),
            None
        );
        assert_eq!(clean_smtp_address("ana@localhost"), None);
    }

    #[test]
    fn validates_https_meeting_links_only() {
        assert_eq!(
            validated_https_meeting_url(Some("https://meet.corp.local/alice/1234>")).as_deref(),
            Some("https://meet.corp.local/alice/1234")
        );
        assert_eq!(
            validated_https_meeting_url(Some("sip:alice@corp.local;gruu")),
            None
        );
        assert_eq!(
            validated_https_meeting_url(Some("http://meet.corp.local/alice/1234")),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "needs a running Classic Outlook session on Windows"]
    fn live_outlook_calendar() {
        if std::env::var_os("LOKALBOT_OUTLOOK_LIVE_TEST").is_none() {
            return;
        }
        let now = Utc::now();
        let options = ParseOptions {
            provider: String::new(),
            from: now - chrono::Duration::days(1),
            to: now + chrono::Duration::days(7),
            my_addresses: Vec::new(),
            calendar_title: None,
            default_zone: None,
        };
        let events = fetch(&options).unwrap().expect("Classic Outlook is open");
        eprintln!("{events:#?}");
        assert!(events.iter().all(|e| e.provider == "outlook"));
    }
}
