//! A small MPEG-DASH reader for single-representation audio manifests (TIDAL's unofficial
//! playback answers with one): the initialization segment, then every media segment in order,
//! read as one fragmented-MP4 stream. Live manifests, several representations and encryption
//! are not handled; an encrypted stream is refused before it gets here.

use quick_xml::events::Event;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashManifest {
    pub codecs: String,
    pub mime_type: String,
    pub sample_rate: Option<u32>,
    /// The initialization segment, then the media segments in play order.
    pub urls: Vec<String>,
}

fn attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// Parse a manifest: the first representation's `SegmentTemplate` with its `SegmentTimeline`
/// (each `S` is one segment plus `r` repeats).
pub fn parse(xml: &str) -> Result<DashManifest, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let (mut codecs, mut mime, mut rate) = (String::new(), String::new(), None);
    let (mut init, mut media, mut start) = (None::<String>, None::<String>, 1u64);
    let mut segments: u64 = 0;
    let mut in_rep = false;
    let mut done_rep = false;
    loop {
        match reader.read_event().map_err(|e| format!("manifest: {e}"))? {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    "AdaptationSet" if mime.is_empty() => {
                        mime = attr(&e, "mimeType").unwrap_or_default();
                    }
                    "Representation" if !done_rep => {
                        in_rep = true;
                        if let Some(c) = attr(&e, "codecs") {
                            codecs = c;
                        }
                        if let Some(m) = attr(&e, "mimeType") {
                            mime = m;
                        }
                        rate = attr(&e, "audioSamplingRate").and_then(|v| v.parse().ok());
                    }
                    "SegmentTemplate" if in_rep || !done_rep => {
                        init = attr(&e, "initialization");
                        media = attr(&e, "media");
                        start = attr(&e, "startNumber").and_then(|v| v.parse().ok()).unwrap_or(1);
                    }
                    "S" if in_rep || !done_rep => {
                        let r: i64 = attr(&e, "r").and_then(|v| v.parse().ok()).unwrap_or(0);
                        segments += 1 + r.max(0) as u64;
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == "Representation" {
                    in_rep = false;
                    done_rep = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let init = init.ok_or("manifest without an initialization segment")?;
    let media = media.ok_or("manifest without a media template")?;
    if segments == 0 {
        return Err("manifest without segments".into());
    }
    let mut urls = vec![init];
    for n in start..start + segments {
        urls.push(media.replace("$Number$", &n.to_string()));
    }
    Ok(DashManifest {
        codecs,
        mime_type: mime,
        sample_rate: rate,
        urls,
    })
}

/// The segments, read one after another as one stream (blocking; for the audio thread).
pub struct DashReader {
    client: &'static reqwest::blocking::Client,
    urls: std::collections::VecDeque<String>,
    current: Option<reqwest::blocking::Response>,
}

impl DashReader {
    pub fn new(client: &'static reqwest::blocking::Client, urls: Vec<String>) -> Self {
        Self {
            client,
            urls: urls.into(),
            current: None,
        }
    }
}

impl std::io::Read for DashReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(r) = self.current.as_mut() {
                let n = r.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.current = None;
            }
            let Some(url) = self.urls.pop_front() else {
                return Ok(0);
            };
            let resp = self
                .client
                .get(&url)
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| std::io::Error::other(format!("segment: {e}")))?;
            self.current = Some(resp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tidal_style_manifest_lists_its_segments() {
        let xml = r#"<?xml version='1.0' encoding='UTF-8'?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT3M2.1S" type="static">
 <Period id="0">
  <AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">
   <Representation id="FLAC,96000,24" codecs="flac" bandwidth="3000000" audioSamplingRate="96000">
    <SegmentTemplate timescale="96000" initialization="https://cdn.example/init.mp4?t=1" media="https://cdn.example/$Number$.mp4?t=1" startNumber="1">
     <SegmentTimeline>
      <S d="383936" r="44"/>
      <S d="214080"/>
     </SegmentTimeline>
    </SegmentTemplate>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>"#;
        let m = parse(xml).unwrap();
        assert_eq!(m.codecs, "flac");
        assert_eq!(m.sample_rate, Some(96_000));
        assert_eq!(m.urls.len(), 1 + 46);
        assert_eq!(m.urls[0], "https://cdn.example/init.mp4?t=1");
        assert_eq!(m.urls[1], "https://cdn.example/1.mp4?t=1");
        assert_eq!(m.urls[46], "https://cdn.example/46.mp4?t=1");
        assert!(parse("<MPD></MPD>").is_err());
    }
}
