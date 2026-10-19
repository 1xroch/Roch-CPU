//! Reads Windows' own restricted-MSR events. Never changes virtualization settings.

use chrono::{DateTime, Local, NaiveDateTime, Utc};

use super::oc_mailbox::OcMailbox;

#[derive(Clone, Debug, PartialEq)]
pub struct RestrictedMsrAccess {
    pub register: u32,
    pub is_write: bool,
    pub time_utc: DateTime<Utc>,
    pub driver: String,
}

pub const LOG_NAME: &str = "Microsoft-Windows-Hyper-V-Hypervisor-Operational";

const EVENT_NS: &str = "http://schemas.microsoft.com/win/2004/08/events/event";

/// Restricted-MSR events (EventID 12550) recorded since this boot, newest first, plus a status
/// line (the C# `out string status`). Events read before a log error are still returned.
pub fn read_current_boot() -> (Vec<RestrictedMsrAccess>, String) {
    let mut found = Vec::new();
    // DateTime.UtcNow - Environment.TickCount64
    let tick = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
    let boot = Utc::now() - chrono::Duration::milliseconds(tick as i64);
    let status = match event_log::read_events(boot, &mut found) {
        Ok(()) => {
            if found.is_empty() {
                "No restricted-MSR events found for this boot; absence is not proof that access is allowed.".to_string()
            } else {
                format!("Windows recorded {} restricted-MSR access event(s) during this boot.", found.len())
            }
        }
        Err(message) => format!("Windows restricted-MSR log could not be read: {message}"),
    };
    (found, status)
}

/// Parses one rendered event (`EvtRenderEventXml`). `None` for anything that is not a
/// well-formed 12550 event from the hypervisor provider created at or after `boot_utc`.
pub fn parse(xml: &str, boot_utc: DateTime<Utc>) -> Option<RestrictedMsrAccess> {
    // XElement.Parse: malformed XML (XmlException) is "no event".
    let root = xml::parse(xml).ok()?;
    let system = root.element(EVENT_NS, "System");
    if system.and_then(|s| s.element(EVENT_NS, "EventID")).map(|e| e.value()).as_deref() != Some("12550")
        || system.and_then(|s| s.element(EVENT_NS, "Provider")).and_then(|p| p.attribute("Name"))
            != Some("Microsoft-Windows-Hyper-V-Hypervisor")
    {
        return None;
    }
    let time = system
        .and_then(|s| s.element(EVENT_NS, "TimeCreated"))
        .and_then(|t| t.attribute("SystemTime"))
        .and_then(parse_utc)?;
    if time < boot_utc {
        return None;
    }
    let field = |name: &str| -> Option<String> {
        root.element(EVENT_NS, "EventData")?
            .elements(EVENT_NS, "Data")
            .find(|e| e.attribute("Name") == Some(name))
            .map(|e| e.value())
    };
    let raw = field("Msr");
    let write = field("IsWrite");
    let (Some(raw), Some(write)) = (raw, write) else { return None };
    if write != "0" && write != "1" {
        return None;
    }
    let hex = raw.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x"));
    let register = parse_uint(if hex { &raw[2..] } else { &raw }, hex)?;
    Some(RestrictedMsrAccess {
        register,
        is_write: write == "1",
        time_utc: time,
        driver: field("ImageName").unwrap_or_else(|| "unknown driver".to_string()),
    })
}

pub fn voltage_block_reason(accesses: &[RestrictedMsrAccess]) -> Option<String> {
    accesses.iter().find(|a| a.register == OcMailbox::MSR_OC_MAILBOX && a.is_write).map(|access| {
        format!(
            "Windows Hyper-V restricted voltage mailbox access (MSR 0x150, event 12550, {}, {}). Voltage controls are unavailable in this boot.",
            access.driver,
            access.time_utc.with_timezone(&Local).format("%H:%M:%S")
        )
    })
}

/// `DateTime.TryParse(…, InvariantCulture, AdjustToUniversal | AssumeUniversal)` for the ISO 8601
/// forms the event log produces: an explicit offset is honoured, none means UTC.
fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, format) {
            return Some(t.and_utc());
        }
    }
    None
}

/// `uint.TryParse` with `NumberStyles.HexNumber` (hex digits, surrounding white space) or
/// `NumberStyles.Integer` (surrounding white space and a leading sign).
fn parse_uint(s: &str, hex: bool) -> Option<u32> {
    let s = s.trim_matches(|c: char| c == ' ' || ('\u{9}'..='\u{D}').contains(&c));
    if hex {
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        // Leading zeros are allowed; the value must fit in 32 bits.
        let digits = s.trim_start_matches('0');
        if digits.len() > 8 {
            return None;
        }
        return Some(if digits.is_empty() { 0 } else { u32::from_str_radix(digits, 16).ok()? });
    }
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: u32 = digits.parse().ok()?;
    if negative && value != 0 {
        return None;
    }
    Some(value)
}

/// The Windows event-log query (wevtapi), kept apart so `parse` stays a pure function.
mod event_log {
    use chrono::{DateTime, Utc};
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS};
    use windows::Win32::System::EventLog::{
        EVT_HANDLE, EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender,
        EvtRenderEventXml,
    };
    use windows::core::{HSTRING, PCWSTR};

    use super::{EVENT_NS, LOG_NAME, RestrictedMsrAccess, parse, parse_utc, xml};

    const INFINITE: u32 = 0xFFFF_FFFF;

    struct Handle(EVT_HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = EvtClose(self.0);
                }
            }
        }
    }

    pub(super) fn read_events(boot: DateTime<Utc>, found: &mut Vec<RestrictedMsrAccess>) -> Result<(), String> {
        let path = HSTRING::from(LOG_NAME);
        let query_text = HSTRING::from("*[System[EventID=12550]]");
        let query = unsafe {
            EvtQuery(
                None,
                PCWSTR(path.as_ptr()),
                PCWSTR(query_text.as_ptr()),
                EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
            )
        }
        .map_err(|e| e.message())?;
        let query = Handle(query);
        for _ in 0..256 {
            let mut events = [0isize; 1];
            let mut returned = 0u32;
            match unsafe { EvtNext(query.0, &mut events, INFINITE, 0, &mut returned) } {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() => break,
                Err(e) => return Err(e.message()),
            }
            if returned == 0 {
                break;
            }
            let record = Handle(EVT_HANDLE(events[0]));
            let xml_text = render_xml(&record)?;
            // record.TimeCreated (the System/TimeCreated value of the same record).
            let created = xml::parse(&xml_text)
                .ok()
                .and_then(|root| {
                    root.element(EVENT_NS, "System")
                        .and_then(|s| s.element(EVENT_NS, "TimeCreated"))
                        .and_then(|t| t.attribute("SystemTime"))
                        .map(|s| s.to_string())
                })
                .and_then(|s| parse_utc(&s));
            if created.is_some_and(|t| t < boot) {
                break;
            }
            if let Some(access) = parse(&xml_text, boot) {
                found.push(access);
            }
        }
        Ok(())
    }

    fn render_xml(record: &Handle) -> Result<String, String> {
        let mut used = 0u32;
        let mut count = 0u32;
        let first = unsafe { EvtRender(None, record.0, EvtRenderEventXml.0, 0, None, &mut used, &mut count) };
        match first {
            Ok(()) => return Ok(String::new()),
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {}
            Err(e) => return Err(e.message()),
        }
        let mut buf = vec![0u16; (used as usize).div_ceil(2) + 1];
        let size = (buf.len() * 2) as u32;
        unsafe {
            EvtRender(None, record.0, EvtRenderEventXml.0, size, Some(buf.as_mut_ptr().cast()), &mut used, &mut count)
        }
        .map_err(|e| e.message())?;
        let chars = (used as usize / 2).min(buf.len());
        let text = &buf[..chars];
        let len = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        Ok(String::from_utf16_lossy(&text[..len]))
    }
}

/// Minimal namespace-aware XML reader: just enough of `XElement.Parse` for rendered event
/// records (elements, attributes, text, entities, CDATA, comments, processing instructions).
/// Anything not well-formed is an error, like `XmlException`; a DTD is refused, as
/// `XElement.Parse` refuses one by default.
mod xml {
    use std::collections::HashMap;

    pub struct Element {
        pub namespace: String,
        pub local: String,
        /// Attributes without a namespace prefix: (local name, value). Namespace declarations
        /// and prefixed attributes are not kept; nothing here looks them up.
        attributes: Vec<(String, String)>,
        children: Vec<Node>,
    }

    enum Node {
        Element(Element),
        Text(String),
    }

    impl Element {
        /// First child element with this expanded name (`XContainer.Element`).
        pub fn element(&self, namespace: &str, local: &str) -> Option<&Element> {
            self.children.iter().find_map(|n| match n {
                Node::Element(e) if e.namespace == namespace && e.local == local => Some(e),
                _ => None,
            })
        }

        /// Child elements with this expanded name (`XContainer.Elements`).
        pub fn elements<'a>(&'a self, namespace: &'a str, local: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
            self.children.iter().filter_map(move |n| match n {
                Node::Element(e) if e.namespace == namespace && e.local == local => Some(e),
                _ => None,
            })
        }

        /// Value of an attribute that has no namespace (`XElement.Attribute(name)`).
        pub fn attribute(&self, local: &str) -> Option<&str> {
            self.attributes.iter().find(|(n, _)| n == local).map(|(_, v)| v.as_str())
        }

        /// Concatenated text of all descendants (`XElement.Value`).
        pub fn value(&self) -> String {
            let mut s = String::new();
            self.collect_text(&mut s);
            s
        }

        fn collect_text(&self, out: &mut String) {
            for n in &self.children {
                match n {
                    Node::Text(t) => out.push_str(t),
                    Node::Element(e) => e.collect_text(out),
                }
            }
        }
    }

    type Scope = HashMap<String, String>;

    struct Parser<'a> {
        s: &'a str,
        pos: usize,
    }

    pub fn parse(text: &str) -> Result<Element, String> {
        let mut p = Parser { s: text.strip_prefix('\u{FEFF}').unwrap_or(text), pos: 0 };
        if p.rest().starts_with("<?xml") {
            p.skip_pi()?;
        }
        p.skip_misc()?;
        if !p.rest().starts_with('<') {
            return Err("Data at the root level is invalid.".into());
        }
        let mut scopes: Vec<Scope> = Vec::new();
        let root = p.parse_element(&mut scopes)?;
        p.skip_misc()?;
        if p.pos != p.s.len() {
            return Err("Unexpected content after the root element.".into());
        }
        Ok(root)
    }

    fn is_name_start(c: char) -> bool {
        c.is_alphabetic() || c == '_' || c == ':' || (c as u32) > 0x7F
    }

    fn is_name_char(c: char) -> bool {
        is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.' || c == '\u{B7}'
    }

    fn is_ws(c: char) -> bool {
        matches!(c, ' ' | '\t' | '\r' | '\n')
    }

    impl<'a> Parser<'a> {
        fn rest(&self) -> &'a str {
            &self.s[self.pos..]
        }

        fn peek(&self) -> Option<char> {
            self.rest().chars().next()
        }

        fn bump(&mut self) -> Option<char> {
            let c = self.peek()?;
            self.pos += c.len_utf8();
            Some(c)
        }

        fn expect(&mut self, token: &str) -> Result<(), String> {
            if self.rest().starts_with(token) {
                self.pos += token.len();
                Ok(())
            } else {
                Err(format!("Expected '{token}'."))
            }
        }

        fn skip_ws(&mut self) -> bool {
            let start = self.pos;
            while self.peek().is_some_and(is_ws) {
                self.pos += 1;
            }
            self.pos != start
        }

        fn skip_until(&mut self, end: &str) -> Result<&'a str, String> {
            match self.rest().find(end) {
                Some(i) => {
                    let body = &self.rest()[..i];
                    self.pos += i + end.len();
                    Ok(body)
                }
                None => Err(format!("Unexpected end of input; expected '{end}'.")),
            }
        }

        fn skip_pi(&mut self) -> Result<(), String> {
            self.expect("<?")?;
            self.skip_until("?>").map(|_| ())
        }

        fn skip_comment(&mut self) -> Result<(), String> {
            self.expect("<!--")?;
            let body = self.skip_until("-->")?;
            if body.contains("--") || body.ends_with('-') {
                return Err("An XML comment cannot contain '--'.".into());
            }
            Ok(())
        }

        /// White space, comments and processing instructions outside the root element.
        fn skip_misc(&mut self) -> Result<(), String> {
            loop {
                self.skip_ws();
                let rest = self.rest();
                if rest.starts_with("<!--") {
                    self.skip_comment()?;
                } else if rest.starts_with("<!DOCTYPE") {
                    return Err("DTD is prohibited in this XML document.".into());
                } else if rest.starts_with("<?") {
                    self.skip_pi()?;
                } else {
                    return Ok(());
                }
            }
        }

        fn parse_name(&mut self) -> Result<&'a str, String> {
            let start = self.pos;
            match self.peek() {
                Some(c) if is_name_start(c) => {
                    self.bump();
                }
                _ => return Err("Name expected.".into()),
            }
            while self.peek().is_some_and(is_name_char) {
                self.bump();
            }
            Ok(&self.s[start..self.pos])
        }

        fn parse_reference(&mut self, out: &mut String) -> Result<(), String> {
            self.expect("&")?;
            let body = self.skip_until(";")?;
            let c = match body {
                "lt" => '<',
                "gt" => '>',
                "amp" => '&',
                "quot" => '"',
                "apos" => '\'',
                _ => {
                    let code = if let Some(h) = body.strip_prefix("#x") {
                        u32::from_str_radix(h, 16).map_err(|_| "Invalid character reference.")?
                    } else if let Some(d) = body.strip_prefix('#') {
                        d.parse::<u32>().map_err(|_| "Invalid character reference.")?
                    } else {
                        return Err(format!("Reference to undeclared entity '{body}'."));
                    };
                    char::from_u32(code).ok_or("Invalid character reference.")?
                }
            };
            out.push(c);
            Ok(())
        }

        fn parse_attribute_value(&mut self) -> Result<String, String> {
            let quote = match self.bump() {
                Some(q @ ('"' | '\'')) => q,
                _ => return Err("Attribute value must be quoted.".into()),
            };
            let mut value = String::new();
            loop {
                match self.peek() {
                    None => return Err("Unexpected end of input in attribute value.".into()),
                    Some(c) if c == quote => {
                        self.bump();
                        return Ok(value);
                    }
                    Some('<') => return Err("'<' is not allowed in an attribute value.".into()),
                    Some('&') => self.parse_reference(&mut value)?,
                    Some(c) => {
                        self.bump();
                        // Attribute-value normalisation.
                        value.push(if is_ws(c) { ' ' } else { c });
                    }
                }
            }
        }

        fn resolve(scopes: &[Scope], prefix: &str) -> Option<String> {
            if prefix == "xml" {
                return Some("http://www.w3.org/XML/1998/namespace".into());
            }
            scopes.iter().rev().find_map(|s| s.get(prefix).cloned())
        }

        fn split(name: &str) -> Result<(&str, &str), String> {
            match name.split_once(':') {
                Some((p, l)) if !p.is_empty() && !l.is_empty() && !l.contains(':') => Ok((p, l)),
                Some(_) => Err(format!("Invalid name '{name}'.")),
                None => Ok(("", name)),
            }
        }

        fn parse_element(&mut self, scopes: &mut Vec<Scope>) -> Result<Element, String> {
            self.expect("<")?;
            let qname = self.parse_name()?;
            let mut raw_attributes: Vec<(&'a str, String)> = Vec::new();
            let empty;
            loop {
                let had_ws = self.skip_ws();
                if self.rest().starts_with("/>") {
                    self.pos += 2;
                    empty = true;
                    break;
                }
                if self.rest().starts_with('>') {
                    self.pos += 1;
                    empty = false;
                    break;
                }
                if !had_ws {
                    return Err("Expected white space between attributes.".into());
                }
                let name = self.parse_name()?;
                self.skip_ws();
                self.expect("=")?;
                self.skip_ws();
                let value = self.parse_attribute_value()?;
                if raw_attributes.iter().any(|(n, _)| *n == name) {
                    return Err(format!("'{name}' is a duplicate attribute name."));
                }
                raw_attributes.push((name, value));
            }

            let mut scope = Scope::new();
            for (name, value) in &raw_attributes {
                if *name == "xmlns" {
                    scope.insert(String::new(), value.clone());
                } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                    if value.is_empty() {
                        return Err("Cannot use a prefix with an empty namespace.".into());
                    }
                    scope.insert(prefix.to_string(), value.clone());
                }
            }
            scopes.push(scope);

            let result = (|| -> Result<Element, String> {
                let (prefix, local) = Self::split(qname)?;
                let namespace = match Self::resolve(scopes, prefix) {
                    Some(ns) => ns,
                    None if prefix.is_empty() => String::new(),
                    None => return Err(format!("'{prefix}' is an undeclared prefix.")),
                };
                let mut attributes = Vec::new();
                for (name, value) in raw_attributes {
                    if name == "xmlns" || name.starts_with("xmlns:") {
                        continue;
                    }
                    let (p, l) = Self::split(name)?;
                    if p.is_empty() {
                        attributes.push((l.to_string(), value));
                    } else if Self::resolve(scopes, p).is_none() {
                        return Err(format!("'{p}' is an undeclared prefix."));
                    }
                }
                let mut element = Element { namespace, local: local.to_string(), attributes, children: Vec::new() };
                if empty {
                    return Ok(element);
                }
                let mut text = String::new();
                loop {
                    let rest = self.rest();
                    if rest.is_empty() {
                        return Err(format!("Unexpected end of input; element '{qname}' is not closed."));
                    }
                    if rest.starts_with("</") {
                        self.pos += 2;
                        let end = self.parse_name()?;
                        self.skip_ws();
                        self.expect(">")?;
                        if end != qname {
                            return Err(format!("The '{qname}' start tag does not match the end tag of '{end}'."));
                        }
                        break;
                    } else if rest.starts_with("<!--") {
                        self.skip_comment()?;
                    } else if rest.starts_with("<![CDATA[") {
                        self.pos += "<![CDATA[".len();
                        text.push_str(self.skip_until("]]>")?);
                    } else if rest.starts_with("<?") {
                        self.skip_pi()?;
                    } else if rest.starts_with('<') {
                        if !text.is_empty() {
                            element.children.push(Node::Text(std::mem::take(&mut text)));
                        }
                        let child = self.parse_element(scopes)?;
                        element.children.push(Node::Element(child));
                    } else if rest.starts_with('&') {
                        self.parse_reference(&mut text)?;
                    } else {
                        let c = self.bump().unwrap_or_default();
                        if c == '\r' {
                            // Line-end normalisation: CR LF and lone CR become LF.
                            if self.peek() == Some('\n') {
                                self.bump();
                            }
                            text.push('\n');
                        } else {
                            text.push(c);
                        }
                    }
                }
                if !text.is_empty() {
                    element.children.push(Node::Text(text));
                }
                Ok(element)
            })();
            scopes.pop();
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const RESTRICTED_EVENT: &str = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System>
<Provider Name="Microsoft-Windows-Hyper-V-Hypervisor"/><EventID>12550</EventID>
<TimeCreated SystemTime="2026-09-16T23:58:10Z"/></System><EventData>
<Data Name="Msr">0x150</Data><Data Name="IsWrite">1</Data><Data Name="ImageName">WinRing0x64.sys</Data>
</EventData></Event>"#;

    fn boot_utc() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 16, 23, 51, 10).unwrap()
    }

    #[test]
    fn windows_event_identifies_a_blocked_voltage_transaction() {
        let access = parse(RESTRICTED_EVENT, boot_utc());
        assert!(
            access.as_ref().is_some_and(|a| a.register == 0x150 && a.is_write && a.driver == "WinRing0x64.sys"),
            "Wrong event data"
        );
        assert!(voltage_block_reason(&[access.unwrap()]).is_some(), "Voltage restriction not reported");
    }

    #[test]
    fn previous_boot_restrictions_do_not_disable_a_new_boot() {
        assert!(parse(RESTRICTED_EVENT, boot_utc() + chrono::Duration::days(1)).is_none(), "Stale restriction retained");
    }

    #[test]
    fn unrelated_register_restrictions_do_not_disable_voltage() {
        let access = parse(&RESTRICTED_EVENT.replace("0x150", "0x607"), boot_utc());
        assert!(voltage_block_reason(&[access.unwrap()]).is_none(), "Unrelated event disabled voltage");
        assert!(
            parse(&RESTRICTED_EVENT.replace("<EventID>12550", "<EventID>99"), boot_utc()).is_none(),
            "Wrong event accepted"
        );
    }

    #[test]
    fn missing_or_malformed_log_data_is_not_treated_as_a_confirmed_block() {
        assert!(parse("invalid xml", boot_utc()).is_none(), "Malformed event accepted");
        assert!(parse(&RESTRICTED_EVENT.replace("0x150", "invalid"), boot_utc()).is_none(), "Invalid register accepted");
        assert!(voltage_block_reason(&[]).is_none(), "Empty log means blocked");
    }

    #[test]
    fn rendered_event_with_fractional_time_and_qualifiers_parses() {
        let xml = RESTRICTED_EVENT
            .replace("2026-09-16T23:58:10Z", "2026-09-16T23:58:10.1234567Z")
            .replace("<EventID>", "<EventID Qualifiers=''>");
        let access = parse(&xml, boot_utc()).unwrap();
        assert_eq!(access.time_utc.timestamp(), Utc.with_ymd_and_hms(2026, 9, 16, 23, 58, 10).unwrap().timestamp());
        assert!(parse("<Event xmlns='x'><System></Event>", boot_utc()).is_none());
    }

    /// Runs the real event-log query (`cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn read_current_boot_smoke() {
        let (found, status) = read_current_boot();
        println!("{status} ({} events)", found.len());
        assert!(!status.is_empty());
    }
}
