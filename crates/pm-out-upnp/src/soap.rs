//! Minimal SOAP 1.1 client for UPnP control actions (blocking).

use crate::UpnpError;
use std::time::Duration;

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// Build the envelope for `action` on `service_type` with `(name, value)` arguments.
pub fn envelope(service_type: &str, action: &str, args: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (k, v) in args {
        body.push_str(&format!("<{k}>{}</{k}>", xml_escape(v)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action} xmlns:u=\"{service_type}\">{body}</u:{action}></s:Body></s:Envelope>"
    )
}

/// Call an action; returns the response body (XML) on success. Runs on a
/// scoped thread so it is safe from any context, including async ones.
pub fn call(
    control_url: &str,
    service_type: &str,
    action: &str,
    args: &[(&str, &str)],
) -> Result<String, UpnpError> {
    let body = envelope(service_type, action, args);
    let url = control_url.to_string();
    let soap_action = format!("\"{service_type}#{action}\"");
    let action_name = action.to_string();
    std::thread::scope(|sc| {
        sc.spawn(move || -> Result<String, UpnpError> {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()?;
            let resp = client
                .post(&url)
                .header("Content-Type", "text/xml; charset=\"utf-8\"")
                .header("SOAPAction", soap_action)
                .body(body)
                .send()?;
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            if !status.is_success() {
                let detail = element_text(&text, "errorDescription")
                    .or_else(|| element_text(&text, "faultstring"))
                    .unwrap_or_else(|| status.to_string());
                return Err(UpnpError::Soap {
                    action: action_name,
                    detail,
                });
            }
            Ok(text)
        })
        .join()
        .unwrap_or_else(|_| Err(UpnpError::Other("SOAP thread panicked".into())))
    })
}

/// The text of the first `<name>…</name>` element (namespace prefixes ignored).
pub fn element_text(xml: &str, name: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    let mut inside = false;
    // The text comes in pieces around each entity (`&amp;` in a URL's query is its own event)
    let mut text: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                inside = e.local_name().as_ref() == name;
            }
            Ok(Event::Text(t)) if inside => text
                .get_or_insert_with(String::new)
                .push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0)),
            Ok(Event::GeneralRef(r)) if inside => {
                let name = r.xml10_content().to_string();
                let piece = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => quick_xml::escape::resolve_xml_entity(&name)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("&{name};")),
                };
                text.get_or_insert_with(String::new).push_str(&piece);
            }
            Ok(Event::End(_)) if inside => return text.map(|t| t.trim().to_string()),
            Ok(Event::End(_)) => inside = false,
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// DIDL-Lite metadata renderers show for the stream: title, artist, album and art when known,
/// and the resource with its full protocolInfo (TVs match it against what they accept).
pub fn didl(meta: &pm_zones::TrackMeta, uri: &str, protocol_info: &str) -> String {
    let mut extra = String::new();
    if let Some(a) = &meta.artist {
        let a = xml_escape(a);
        extra.push_str(&format!(
            "<dc:creator>{a}</dc:creator><upnp:artist>{a}</upnp:artist>"
        ));
    }
    if let Some(a) = &meta.album {
        extra.push_str(&format!("<upnp:album>{}</upnp:album>", xml_escape(a)));
    }
    if let Some(i) = &meta.image {
        extra.push_str(&format!(
            "<upnp:albumArtURI dlna:profileID=\"JPEG_TN\">{}</upnp:albumArtURI>",
            xml_escape(i)
        ));
    }
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\">\
<item id=\"pm-stream\" parentID=\"0\" restricted=\"1\"><dc:title>{}</dc:title>{extra}<upnp:class>object.item.audioItem.musicTrack</upnp:class>\
<res protocolInfo=\"{}\">{}</res></item></DIDL-Lite>",
        xml_escape(meta.title.as_deref().unwrap_or("Pubky Music")),
        xml_escape(protocol_info),
        xml_escape(uri)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_and_parsing() {
        let e = envelope(
            crate::AV_TRANSPORT,
            "SetAVTransportURI",
            &[("InstanceID", "0"), ("CurrentURI", "http://h/s.wav?a=1&b=<2>")],
        );
        assert!(
            e.contains("<u:SetAVTransportURI xmlns=\"urn:schemas-upnp-org:service:AVTransport:1\">")
                || e.contains("<u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">")
        );
        assert!(e.contains("<CurrentURI>http://h/s.wav?a=1&amp;b=&lt;2&gt;</CurrentURI>"));
        // Read back whole: the text around each entity, and the entities
        assert_eq!(
            element_text(&e, "CurrentURI").as_deref(),
            Some("http://h/s.wav?a=1&b=<2>")
        );
        assert_eq!(
            element_text("<a><b>x&#38;y &#x26; z</b></a>", "b").as_deref(),
            Some("x&y & z")
        );
        let resp = "<s:Envelope xmlns:s=\"x\"><s:Body><u:GetVolumeResponse><CurrentVolume>37</CurrentVolume></u:GetVolumeResponse></s:Body></s:Envelope>";
        assert_eq!(element_text(resp, "CurrentVolume").as_deref(), Some("37"));
        assert!(element_text(resp, "Nope").is_none());
        let d = didl(
            &pm_zones::TrackMeta::default(),
            "http://h/s.wav",
            "http-get:*:audio/wav:*",
        );
        assert!(d.contains("<dc:title>Pubky Music</dc:title>") && d.contains("http-get:*:audio/wav:*"));
        let m = pm_zones::TrackMeta {
            title: Some("Bohemian Rhapsody".into()),
            artist: Some("Queen".into()),
            album: Some("A Night at the Opera".into()),
            image: Some("http://n/v1/art/7?k=a&b".into()),
        };
        let d = didl(&m, "http://h/s.wav", "http-get:*:audio/wav:*");
        assert!(d.contains("<upnp:artist>Queen</upnp:artist>"));
        assert!(d.contains("<upnp:album>A Night at the Opera</upnp:album>"));
        assert!(d.contains(
            "<upnp:albumArtURI dlna:profileID=\"JPEG_TN\">http://n/v1/art/7?k=a&amp;b</upnp:albumArtURI>"
        ));
    }
}
