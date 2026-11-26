//! Facts keep the id they were recorded with, so an unmerge is exact.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use deadpool_postgres::GenericClient;

use crate::api::events::repo_url;

pub const MAX_LINKS: i64 = 1000;

pub struct Link {
    pub from: String,
    pub to: String,
    pub source: String,
    pub at: f64,
}

/// A move makes its target a project of its own as of then (so a rename back works); an unmerge frees the id.
pub fn resolve(links: &[Link]) -> BTreeMap<String, String> {
    let mut sorted: Vec<&Link> = links.iter().filter(|l| l.from != l.to).collect();
    sorted.sort_by(|a, b| a.at.total_cmp(&b.at).then_with(|| (&a.from, &a.to, &a.source).cmp(&(&b.from, &b.to, &b.source))));
    let mut parent: HashMap<&str, &str> = HashMap::new();
    for l in sorted {
        if l.source == "split" {
            parent.remove(l.from.as_str());
        } else {
            parent.remove(l.to.as_str());
            parent.insert(&l.from, &l.to);
        }
    }
    let mut out = BTreeMap::new();
    for &id in parent.keys() {
        let mut cur = id;
        let mut seen = HashSet::from([id]);
        while let Some(&next) = parent.get(cur) {
            if !seen.insert(next) {
                break;
            }
            cur = next;
        }
        if cur != id {
            out.insert(id.to_string(), cur.to_string());
        }
    }
    out
}

pub async fn map(c: &impl GenericClient, user_id: i64) -> Result<HashMap<String, String>> {
    let rows = c.query("SELECT alias, canonical FROM project_aliases WHERE user_id = $1", &[&user_id]).await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

pub fn canonical<'a>(map: &'a HashMap<String, String>, id: &'a str) -> &'a str {
    map.get(id).map_or(id, String::as_str)
}

pub async fn canonical_list(c: &impl GenericClient, user_id: i64, ids: &[String]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let map = map(c, user_id).await?;
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        let id = canonical(&map, id);
        if !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

/// Sending the same pair again only moves its time forward.
pub async fn link(tx: &impl GenericClient, user_id: i64, links: &[(String, String, f64)], source: &str) -> Result<()> {
    let mut latest: BTreeMap<(&str, &str), f64> = BTreeMap::new();
    for (from, to, at) in links {
        let e = latest.entry((from, to)).or_insert(*at);
        *e = e.max(*at);
    }
    let from: Vec<&str> = latest.keys().map(|k| k.0).collect();
    let to: Vec<&str> = latest.keys().map(|k| k.1).collect();
    let at: Vec<f64> = latest.values().copied().collect();
    tx.execute(
        "INSERT INTO project_links (user_id, from_id, to_id, source, at)
         SELECT $1, f, t, $5::text, a FROM UNNEST($2::text[], $3::text[], $4::float8[]) AS x(f, t, a)
         ON CONFLICT (user_id, from_id, to_id, source) DO UPDATE SET at = GREATEST(project_links.at, EXCLUDED.at)",
        &[&user_id, &from, &to, &at, &source],
    )
    .await?;
    Ok(())
}

/// A group with a hidden project is hidden under its canonical id too, and nothing is ever unhidden here.
/// The caller holds the user row lock.
pub async fn refresh(tx: &impl GenericClient, user_id: i64) -> Result<bool> {
    let links: Vec<Link> = tx
        .query("SELECT from_id, to_id, source, at FROM project_links WHERE user_id = $1", &[&user_id])
        .await?
        .iter()
        .map(|r| Link { from: r.get(0), to: r.get(1), source: r.get(2), at: r.get(3) })
        .collect();
    let wanted = resolve(&links);
    let have: BTreeMap<String, String> = map(tx, user_id).await?.into_iter().collect();
    let changed = wanted != have;
    if changed {
        let aliases: Vec<&str> = wanted.keys().map(String::as_str).collect();
        let canonicals: Vec<&str> = wanted.values().map(String::as_str).collect();
        tx.execute("DELETE FROM project_aliases WHERE user_id = $1", &[&user_id]).await?;
        tx.execute(
            "INSERT INTO project_aliases (user_id, alias, canonical)
             SELECT $1, a, c FROM UNNEST($2::text[], $3::text[]) AS x(a, c)",
            &[&user_id, &aliases, &canonicals],
        )
        .await?;
    }
    let hidden = tx
        .query(
            "SELECT a.canonical, min(m.first_seen), max(m.last_seen)
             FROM project_aliases a JOIN projects m ON m.user_id = a.user_id AND m.id = a.alias
             WHERE a.user_id = $1 GROUP BY a.canonical HAVING bool_or(m.hidden)",
            &[&user_id],
        )
        .await?;
    for r in &hidden {
        let id: String = r.get(0);
        tx.execute(
            "INSERT INTO projects (user_id, id, repo_url, hidden, first_seen, last_seen) VALUES ($1, $2, $3, TRUE, $4, $5)
             ON CONFLICT (user_id, id) DO UPDATE SET hidden = TRUE, first_seen = LEAST(projects.first_seen, EXCLUDED.first_seen),
                last_seen = GREATEST(projects.last_seen, EXCLUDED.last_seen)",
            &[&user_id, &id, &repo_url(&id), &r.get::<_, f64>(1), &r.get::<_, f64>(2)],
        )
        .await?;
    }
    Ok(changed)
}

/// A device's sighting of its own commit wins over one that arrived by pull, then the first seen.
macro_rules! canon_commits {
    () => {
        "(SELECT DISTINCT ON (COALESCE(a.canonical, c.project), c.sha) c.user_id, COALESCE(a.canonical, c.project) AS project,
                 c.sha, c.ts, c.files, c.added, c.removed, c.device_id, c.authored_at, c.committed_at, c.via, c.changes, c.not_before
          FROM commits c LEFT JOIN project_aliases a ON a.user_id = c.user_id AND a.alias = c.project
          WHERE c.user_id = $1
          ORDER BY COALESCE(a.canonical, c.project), c.sha, (c.via IS NOT DISTINCT FROM 'arrived'), c.ts)"
    };
}
pub(crate) use canon_commits;

macro_rules! canon_projects {
    () => {
        "(SELECT p.id, p.repo_url, p.hidden, g.first_seen, g.last_seen FROM projects p
          JOIN (SELECT COALESCE(a.canonical, m.id) AS id, min(m.first_seen) AS first_seen, max(m.last_seen) AS last_seen
                FROM projects m LEFT JOIN project_aliases a ON a.user_id = m.user_id AND a.alias = m.id
                WHERE m.user_id = $1 GROUP BY 1) g ON g.id = p.id
          WHERE p.user_id = $1)"
    };
}
pub(crate) use canon_projects;

#[cfg(test)]
mod tests {
    use super::*;

    fn l(from: &str, to: &str, source: &str, at: f64) -> Link {
        Link { from: from.into(), to: to.into(), source: source.into(), at }
    }

    fn resolved(links: &[Link]) -> Vec<(String, String)> {
        resolve(links).into_iter().collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn a_chain_resolves_to_its_end() {
        let out = resolved(&[l("a", "b", "device", 1.0), l("b", "c", "device", 2.0)]);
        assert_eq!(out, pairs(&[("a", "c"), ("b", "c")]));
    }

    #[test]
    fn the_order_links_arrive_in_does_not_matter() {
        let out = resolved(&[l("b", "c", "device", 2.0), l("a", "b", "device", 1.0)]);
        assert_eq!(out, pairs(&[("a", "c"), ("b", "c")]));
    }

    #[test]
    fn renaming_back_makes_the_old_id_canonical_again() {
        let out = resolved(&[l("a", "b", "device", 1.0), l("b", "a", "device", 2.0)]);
        assert_eq!(out, pairs(&[("b", "a")]));
    }

    #[test]
    fn a_pair_sent_again_later_wins() {
        let out = resolved(&[l("a", "b", "device", 1.0), l("b", "a", "device", 2.0), l("a", "b", "device", 3.0)]);
        assert_eq!(out, pairs(&[("a", "b")]));
    }

    #[test]
    fn a_split_frees_a_merged_id() {
        let out = resolved(&[l("a", "b", "merge", 1.0), l("a", "b", "split", 2.0)]);
        assert!(out.is_empty());
    }

    #[test]
    fn a_split_only_frees_the_id_it_names() {
        let out = resolved(&[l("a", "b", "device", 1.0), l("b", "c", "merge", 2.0), l("a", "c", "split", 3.0)]);
        assert_eq!(out, pairs(&[("b", "c")]));
    }

    #[test]
    fn a_later_device_link_applies_after_a_split() {
        let out = resolved(&[l("a", "b", "merge", 1.0), l("a", "b", "split", 2.0), l("a", "b", "device", 3.0)]);
        assert_eq!(out, pairs(&[("a", "b")]));
    }

    #[test]
    fn a_stale_device_link_stays_undone() {
        let out = resolved(&[l("a", "b", "split", 5.0), l("a", "b", "device", 2.0)]);
        assert!(out.is_empty());
    }
}
