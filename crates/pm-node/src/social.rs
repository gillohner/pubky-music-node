//! `pm-node uri`, `pm-node pinakes …` and `pm-node discover …`: the M9/M10
//! groundwork on the CLI. Read-only against the shared Pinakes and Nexus.

use anyhow::Context;
use pm_model::MusicUri;
use pm_pubky::{Discovery, NexusClient, PinakesClient, PmPubkyConfig, SearchQuery};

pub fn uri(input: &str) -> anyhow::Result<()> {
    let u = MusicUri::parse(input).with_context(|| format!("parsing {input}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "url": u.to_url(),
            "entity": u.entity(),
            "canonical": u.is_canonical(),
            "mbid": u.mbid(),
            "parsed": u,
        }))?
    );
    Ok(())
}

fn clients(cfg: &PmPubkyConfig) -> anyhow::Result<(PinakesClient, NexusClient)> {
    Ok((PinakesClient::new(cfg)?, NexusClient::new(cfg)?))
}

pub async fn pinakes_info(cfg: &PmPubkyConfig) -> anyhow::Result<()> {
    let (p, _) = clients(cfg)?;
    println!("{}", serde_json::to_string_pretty(&p.info().await?)?);
    Ok(())
}

pub async fn pinakes_lookup(cfg: &PmPubkyConfig, uris: &[String]) -> anyhow::Result<()> {
    let (p, _) = clients(cfg)?;
    // Normalize through MusicUri where we can, so the spelling matches what the node tags.
    let uris: Vec<String> = uris
        .iter()
        .map(|u| {
            MusicUri::parse(u)
                .map(|m| m.to_url())
                .unwrap_or_else(|_| u.clone())
        })
        .collect();
    let cards = p.lookup(&uris).await?;
    println!("{}", serde_json::to_string_pretty(&cards)?);
    Ok(())
}

pub async fn pinakes_search(cfg: &PmPubkyConfig, tags: &str, limit: usize) -> anyhow::Result<()> {
    let (p, _) = clients(cfg)?;
    let mut q = SearchQuery::tags(tags);
    q.limit = Some(limit);
    let res = p.search(&q).await?;
    println!("{}", serde_json::to_string_pretty(&res)?);
    Ok(())
}

pub async fn discover_trends(cfg: &PmPubkyConfig, limit: usize, window: &str) -> anyhow::Result<()> {
    let (p, n) = clients(cfg)?;
    let d = Discovery::new(p, n);
    match d.trending(window, limit).await {
        Ok(page) => {
            println!("trending in the last {window}:");
            print_items(&page.items);
            return Ok(());
        }
        Err(pm_pubky::discovery::DiscoveryError::Pinakes(pm_pubky::PinakesError::BadRequest(m))) => {
            println!("(sort=trending not on this Pinakes yet: {m}; showing all-time activity)");
        }
        Err(e) => return Err(e.into()),
    }
    let (facets, items) = d.trends(limit).await?;
    println!("top tags:");
    for f in facets.iter().take(15) {
        println!("  {:<24} {}", f.label, f.count);
    }
    println!("items:");
    print_items(&items);
    Ok(())
}

pub async fn discover_key(cfg: &PmPubkyConfig, key: &str) -> anyhow::Result<()> {
    let (p, n) = clients(cfg)?;
    let d = Discovery::new(p, n);
    match d.tagged_anywhere(key, 50).await {
        Ok(page) => print_items(&page.items),
        Err(pm_pubky::discovery::DiscoveryError::Pinakes(pm_pubky::PinakesError::BadRequest(m))) => {
            println!("(key= search not on this Pinakes yet: {m})")
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub async fn discover_by_tag(cfg: &PmPubkyConfig, label: &str, limit: usize) -> anyhow::Result<()> {
    let (p, n) = clients(cfg)?;
    let d = Discovery::new(p, n);
    let page = d.by_tag(label, limit, None).await?;
    print_items(&page.items);
    Ok(())
}

pub async fn discover_friends(cfg: &PmPubkyConfig, viewer: &str, limit: usize) -> anyhow::Result<()> {
    let (p, n) = clients(cfg)?;
    let d = Discovery::new(p, n);
    let page = d.friends_activity(viewer, limit, None).await?;
    if page.reach_truncated {
        println!("(reach truncated)");
    }
    print_items(&page.items);
    Ok(())
}

fn print_items(items: &[pm_pubky::DiscoveredItem]) {
    for it in items {
        println!(
            "  {:<16} {:<40} {}",
            it.card.type_.as_deref().unwrap_or("-"),
            it.card
                .name
                .as_deref()
                .unwrap_or("-")
                .chars()
                .take(40)
                .collect::<String>(),
            it.uri.to_url()
        );
    }
}
