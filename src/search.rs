use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use crate::library::Folder;

pub fn filter_tree(root: &Folder, query: &str) -> Folder {
    let q = query.trim();
    if q.is_empty() {
        return root.clone();
    }
    let matcher = SkimMatcherV2::default();
    filter_folder(root, q, &matcher).unwrap_or_else(|| Folder {
        name: root.name.clone(),
        relpath: root.relpath.clone(),
        kind: root.kind,
        children: Vec::new(),
    })
}

fn name_matches(name: &str, query: &str, matcher: &SkimMatcherV2) -> bool {
    if name.is_empty() {
        return false;
    }
    matcher.fuzzy_match(name, query).is_some()
}

fn filter_folder(folder: &Folder, query: &str, matcher: &SkimMatcherV2) -> Option<Folder> {
    let self_hit = name_matches(&folder.name, query, matcher);
    match folder.kind {
        crate::library::Kind::Album => {
            if self_hit {
                Some(folder.clone())
            } else {
                None
            }
        }
        crate::library::Kind::Collection => {
            if self_hit {
                // Collection name matches: keep the whole branch.
                return Some(folder.clone());
            }
            let children: Vec<Folder> = folder
                .children
                .iter()
                .filter_map(|c| filter_folder(c, query, matcher))
                .collect();
            if children.is_empty() {
                None
            } else {
                Some(Folder {
                    name: folder.name.clone(),
                    relpath: folder.relpath.clone(),
                    kind: folder.kind,
                    children,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Kind;
    use std::path::PathBuf;

    fn album(name: &str) -> Folder {
        Folder {
            name: name.into(),
            relpath: PathBuf::from(name),
            kind: Kind::Album,
            children: vec![],
        }
    }

    fn collection(name: &str, children: Vec<Folder>) -> Folder {
        Folder {
            relpath: PathBuf::from(name),
            name: name.into(),
            kind: Kind::Collection,
            children,
        }
    }

    fn root(children: Vec<Folder>) -> Folder {
        Folder {
            name: String::new(),
            relpath: PathBuf::new(),
            kind: Kind::Collection,
            children,
        }
    }

    #[test]
    fn album_match_keeps_ancestors() {
        let tree = root(vec![collection(
            "2025",
            vec![album("Etyek"), album("Küchenschrank")],
        )]);
        let filtered = filter_tree(&tree, "ety");
        assert_eq!(filtered.children.len(), 1);
        assert_eq!(filtered.children[0].name, "2025");
        assert_eq!(filtered.children[0].children.len(), 1);
        assert_eq!(filtered.children[0].children[0].name, "Etyek");
    }

    #[test]
    fn collection_match_keeps_all_albums() {
        let tree = root(vec![collection(
            "2026",
            vec![album("Lumina Park"), album("Other")],
        )]);
        let filtered = filter_tree(&tree, "2026");
        assert_eq!(filtered.children[0].children.len(), 2);
    }
}
