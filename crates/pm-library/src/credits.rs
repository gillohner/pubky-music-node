//! Artist credits as people write them in tags ("Sefa, Dr. Peacock", "Sefa & Cyclon",
//! "Dr. Peacock feat. MC Lenny") split into the artists they name (owner, 2026-10-22: the Artists
//! page listed "Dr. Peacock, Sefa" and "Sefa, Dr. Peacock" as artists of their own).
//!
//! Joiners: `,` `;` `/` ` & ` ` and ` ` x ` ` vs ` `feat.` `ft.` `featuring` `with`. A credit is
//! kept whole when it is a known act whose name contains a joiner (Simon & Garfunkel, Earth,
//! Wind & Fire…); a file whose MusicBrainz artist ids say one artist is kept whole by the caller.

/// Acts whose names contain a joiner, compared without case. Short and well known; MusicBrainz
/// ids settle the rest.
const WHOLE: &[&str] = &[
    "simon & garfunkel",
    "earth, wind & fire",
    "crosby, stills, nash & young",
    "crosby, stills & nash",
    "tyler, the creator",
    "hall & oates",
    "daryl hall & john oates",
    "sly & the family stone",
    "kool & the gang",
    "peter, paul and mary",
    "emerson, lake & palmer",
    "blood, sweat & tears",
    "derek & the dominos",
    "florence + the machine",
    "mumford & sons",
    "she & him",
    "salt-n-pepa",
    "brooks & dunn",
    "ike & tina turner",
    "the mamas & the papas",
    "marina and the diamonds",
    "chase & status",
    "above & beyond",
    "matt & kim",
    "angus & julia stone",
    "dimitri vegas & like mike",
    "w&w",
    "ac/dc",
];

/// The artists a credit names, in order, without duplicates.
pub fn split_credit(credit: &str) -> Vec<String> {
    let c = credit.trim();
    if c.is_empty() {
        return Vec::new();
    }
    if WHOLE.contains(&c.to_lowercase().as_str()) {
        return vec![c.to_string()];
    }
    // Normalise the word joiners to one mark, then split on it and the punctuation joiners
    let mut s = format!(" {c} ");
    for j in [
        " feat. ",
        " feat ",
        " ft. ",
        " ft ",
        " featuring ",
        " Feat. ",
        " Feat ",
        " Ft. ",
        " FT. ",
        " Featuring ",
        " & ",
        " x ",
        " X ",
        " vs. ",
        " vs ",
        " Vs. ",
        " VS ",
        " with ",
        " With ",
        " and ",
        " And ",
        "; ",
        ";",
        " / ",
    ] {
        s = s.replace(j, "\u{1f}");
    }
    let mut out: Vec<String> = Vec::new();
    for part in s.split(['\u{1f}', ',']) {
        let p = part.trim().trim_matches(|ch: char| ch == '(' || ch == ')').trim();
        if p.is_empty() {
            continue;
        }
        // A piece that is itself a known act stays whole ("Earth, Wind & Fire" inside a credit
        // is rare; keep it simple)
        if !out.iter().any(|o| o.eq_ignore_ascii_case(p)) {
            out.push(p.to_string());
        }
    }
    // "and" joins too often inside names ("Marina and the Diamonds"): only split on it when the
    // credit also has another joiner
    let has_other = [",", " & ", " feat", " ft", " x ", " vs", ";", " / "]
        .iter()
        .any(|j| c.to_lowercase().contains(j));
    if !has_other
        && (c.contains(" and ") || c.contains(" And ") || c.contains(" with ") || c.contains(" With "))
    {
        return vec![c.to_string()];
    }
    if out.is_empty() {
        vec![c.to_string()]
    } else {
        out
    }
}

/// Split every credit of a list (a file may already carry one artist per tag value).
pub fn split_all(credits: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in credits {
        for a in split_credit(c) {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&a)) {
                out.push(a);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_split_into_artists() {
        assert_eq!(split_credit("Sefa, Dr. Peacock"), vec!["Sefa", "Dr. Peacock"]);
        assert_eq!(
            split_credit("Dr. Peacock, Sefa, MC Lenny"),
            vec!["Dr. Peacock", "Sefa", "MC Lenny"]
        );
        assert_eq!(split_credit("Sefa & Cyclon"), vec!["Sefa", "Cyclon"]);
        assert_eq!(
            split_credit("Dr. Peacock feat. MC Lenny"),
            vec!["Dr. Peacock", "MC Lenny"]
        );
        assert_eq!(
            split_credit("Avicii x Nicky Romero"),
            vec!["Avicii", "Nicky Romero"]
        );
        assert_eq!(split_credit("Queen"), vec!["Queen"]);
        assert_eq!(split_credit("Simon & Garfunkel"), vec!["Simon & Garfunkel"]);
        assert_eq!(split_credit("Earth, Wind & Fire"), vec!["Earth, Wind & Fire"]);
        assert_eq!(split_credit("Tyler, The Creator"), vec!["Tyler, The Creator"]);
        assert_eq!(
            split_credit("Marina and the Diamonds"),
            vec!["Marina and the Diamonds"]
        );
        assert_eq!(
            split_credit("Florence + the Machine"),
            vec!["Florence + the Machine"]
        );
        assert_eq!(
            split_all(&["Sefa, Dr. Peacock".into(), "Dr. Peacock".into()]),
            vec!["Sefa", "Dr. Peacock"]
        );
    }
}
