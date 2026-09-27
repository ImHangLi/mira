//! Catalog search ranking, shared by the host's `catalog.list` query and the TUI filter.
//!
//! The query is split into lowercase words. An entry matches when any word matches its
//! ref, local id, title, tags, or description. Entries rank by their best match tier:
//! exact ref or id, exact title, ref/id/title prefix, ref/id/title substring, tag, then
//! description. Within a tier, entries that match more words rank higher. The whole query
//! also counts as one phrase for the exact ref, id, and title tiers. Callers sort stably,
//! so ties keep catalog order.

/// Match tiers, best first.
const EXACT_REF: u8 = 0;
const EXACT_TITLE: u8 = 1;
const PREFIX: u8 = 2;
const SUBSTRING: u8 = 3;
const TAG: u8 = 4;
const DESCRIPTION: u8 = 5;

/// The searchable text of one catalog entry.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub item_ref: &'a str,
    /// The local id: the part of the ref after the plugin.
    pub id: &'a str,
    pub title: &'a str,
    pub tags: &'a [String],
    pub description: &'a str,
}

/// Sort key of a matching entry; a smaller rank sorts first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    tier: u8,
    unmatched_words: usize,
}

/// Splits a query into lowercase words.
pub fn query_words(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// How well `entry` matches the lowercase query `words`; `None` when no word matches.
/// An empty query matches every entry with the same rank.
pub fn rank(words: &[String], entry: &Entry<'_>) -> Option<Rank> {
    if words.is_empty() {
        return Some(Rank::default());
    }
    let item_ref = entry.item_ref.to_lowercase();
    let id = entry.id.to_lowercase();
    let title = entry.title.to_lowercase();
    let tags: Vec<String> = entry.tags.iter().map(|t| t.to_lowercase()).collect();
    let description = entry.description.to_lowercase();
    let names = [&item_ref, &id, &title];
    let tier = |word: &str| {
        if item_ref == word || id == word {
            Some(EXACT_REF)
        } else if title == word {
            Some(EXACT_TITLE)
        } else if names.iter().any(|n| n.starts_with(word)) {
            Some(PREFIX)
        } else if names.iter().any(|n| n.contains(word)) {
            Some(SUBSTRING)
        } else if tags.iter().any(|t| t.contains(word)) {
            Some(TAG)
        } else if description.contains(word) {
            Some(DESCRIPTION)
        } else {
            None
        }
    };
    let tiers: Vec<u8> = words.iter().filter_map(|w| tier(w)).collect();
    let mut best = tiers.iter().copied().min()?;
    if words.len() > 1 {
        let phrase = words.join(" ");
        if item_ref == phrase || id == phrase {
            best = EXACT_REF;
        } else if title == phrase {
            best = best.min(EXACT_TITLE);
        }
    }
    Some(Rank {
        tier: best,
        unmatched_words: words.len() - tiers.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item {
        item_ref: &'static str,
        title: &'static str,
        description: &'static str,
        tags: Vec<String>,
    }

    fn item(item_ref: &'static str, title: &'static str, description: &'static str) -> Item {
        Item {
            item_ref,
            title,
            description,
            tags: vec!["lint".into(), "check".into()],
        }
    }

    fn catalog() -> Vec<Item> {
        vec![
            item("dev.test", "Tests", "Run the suite; lint first"),
            item("dev.typecheck", "Type check", "Check types"),
            item("dev.check", "Check", "All checks"),
            item("dev.lint", "Lint", "Lint the code"),
            Item {
                tags: Vec::new(),
                ..item("perf.slow", "Slowest tests", "Find slow tests")
            },
        ]
    }

    /// The refs that match `query`, best first; ties keep catalog order.
    fn search(query: &str) -> Vec<&'static str> {
        let words = query_words(query);
        let mut ranked: Vec<(Rank, &'static str)> = catalog()
            .iter()
            .filter_map(|i| {
                let entry = Entry {
                    item_ref: i.item_ref,
                    id: i.item_ref.split_once('.').map_or(i.item_ref, |(_, id)| id),
                    title: i.title,
                    tags: &i.tags,
                    description: i.description,
                };
                rank(&words, &entry).map(|r| (r, i.item_ref))
            })
            .collect();
        ranked.sort_by_key(|(r, _)| *r);
        ranked.into_iter().map(|(_, r)| r).collect()
    }

    #[test]
    fn exact_id_ranks_first() {
        let got = search("lint");
        assert_eq!(got[0], "dev.lint");
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn exact_id_before_substring() {
        let got = search("check");
        let check = got.iter().position(|r| *r == "dev.check");
        let typecheck = got.iter().position(|r| *r == "dev.typecheck");
        assert_eq!(check, Some(0));
        assert!(typecheck > check);
    }

    #[test]
    fn any_word_matches_and_phrase_hits_title() {
        let got = search("Slowest tests");
        assert_eq!(got[0], "perf.slow");
        assert!(got.contains(&"dev.test"));
    }

    #[test]
    fn empty_query_keeps_all_and_unknown_words_match_nothing() {
        assert_eq!(search("").len(), 5);
        assert!(search("zzz").is_empty());
    }
}
