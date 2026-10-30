//! Page order: natural sort on the whole entry name, as raw bytes.

use std::cmp::Ordering;

/// Compare two entry names "naturally": a run of ASCII digits is a number
/// (compared by value, leading zeros ignored), everything else is compared
/// byte by byte. The whole name counts, folders included, so `a/2.jpg` comes
/// before `b/1.jpg`. Names that compare equal that way (`01.jpg`, `1.jpg`)
/// are ordered by their bytes, so the order is total and deterministic.
/// Names are bytes, not text: a CP437 name sorts the same on every machine.
pub fn natural_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (ra, rb) = (digit_run(a, i), digit_run(b, j));
            let (na, nb) = (strip_zeros(&a[i..ra]), strip_zeros(&b[j..rb]));
            // More significant digits is the larger number; equal length
            // compares digit by digit, which is numeric order.
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
            i = ra;
            j = rb;
        } else {
            if a[i] != b[j] {
                return a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
    }
    // One name is a prefix of the other (as tokens): the shorter first;
    // otherwise the raw bytes decide.
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b))
}

/// End of the digit run starting at `from`.
fn digit_run(s: &[u8], from: usize) -> usize {
    from + s[from..].iter().take_while(|c| c.is_ascii_digit()).count()
}

fn strip_zeros(d: &[u8]) -> &[u8] {
    let z = d.iter().take_while(|&&c| c == b'0').count();
    &d[z..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_cmp_orders_numbers_by_value() {
        assert_eq!(natural_cmp(b"2.jpg", b"10.jpg"), Ordering::Less);
        assert_eq!(natural_cmp(b"p9", b"p10"), Ordering::Less);
        assert_eq!(natural_cmp(b"p10", b"p9"), Ordering::Greater);
        assert_eq!(natural_cmp(b"p007", b"p10"), Ordering::Less);
        assert_eq!(natural_cmp(b"p1a", b"p1b"), Ordering::Less);
    }

    #[test]
    fn natural_cmp_ties_on_bytes() {
        // Equal as numbers: the byte order decides, both ways round.
        assert_eq!(
            natural_cmp(b"01.jpg", b"1.jpg"),
            b"01.jpg"[..].cmp(&b"1.jpg"[..])
        );
        assert_eq!(
            natural_cmp(b"1.jpg", b"01.jpg"),
            b"1.jpg"[..].cmp(&b"01.jpg"[..])
        );
        assert_eq!(natural_cmp(b"1.jpg", b"1.jpg"), Ordering::Equal);
        assert_ne!(natural_cmp(b"001", b"01"), Ordering::Equal);
        // A name that ends inside the other sorts first.
        assert_eq!(natural_cmp(b"a", b"a1"), Ordering::Less);
    }

    #[test]
    fn natural_cmp_uses_the_full_path() {
        assert_eq!(natural_cmp(b"a/2.jpg", b"b/1.jpg"), Ordering::Less);
        assert_eq!(natural_cmp(b"ch2/1.jpg", b"ch10/1.jpg"), Ordering::Less);
    }

    #[test]
    fn non_utf8_entry_names_sort_by_bytes() {
        // CP437 names: 0x82 (e-acute) and 0x81 (u-umlaut) are not UTF-8.
        let a: &[u8] = b"p\x81.jpg";
        let b: &[u8] = b"p\x82.jpg";
        assert_eq!(natural_cmp(a, b), Ordering::Less);
        assert_eq!(natural_cmp(b"p\x82\xff9", b"p\x82\xff10"), Ordering::Less);
        let mut v: Vec<&[u8]> = vec![b"\xff2", b"\xff10", b"\x80", b"1"];
        v.sort_by(|x, y| natural_cmp(x, y));
        assert_eq!(v, [&b"1"[..], b"\x80", b"\xff2", b"\xff10"]);
    }

    #[test]
    fn huge_digit_runs_do_not_overflow() {
        let a = [b"1".as_slice(), &[b'0'; 400]].concat();
        let b = [b"9".as_slice(), &[b'0'; 399]].concat();
        assert_eq!(natural_cmp(&a, &b), Ordering::Greater);
    }
}
