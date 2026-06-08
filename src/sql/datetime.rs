//! Parser RFC 3339 mínimo a mano, sin dependencias: convierte el literal de
//! `AS OF TIMESTAMP '…'` a epoch ms UTC (docs/04-sql). Solo el subconjunto que
//! una consulta necesita: `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`, con `T`/`t`/
//! espacio como separador. El timestamp es informativo (la versión es la
//! autoridad, D12): un instante anterior a 1970 se satura a 0 (estado génesis).

/// `None` si la cadena no es un RFC 3339 bien formado del subconjunto aceptado.
/// El resultado se satura a 0 para instantes anteriores a la época Unix.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    // Mínimo: `YYYY-MM-DDTHH:MM:SS` (19) + zona obligatoria `Z` (20).
    if b.len() < 20 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let year = parse_n(&b[0..4])?;
    let month = parse_n(&b[5..7])?;
    let day = parse_n(&b[8..10])?;
    let hour = parse_n(&b[11..13])?;
    let min = parse_n(&b[14..16])?;
    let sec = parse_n(&b[17..19])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }

    // Fracción de segundo opcional: se trunca a milisegundos.
    let mut i = 19;
    let mut millis: i64 = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        let mut digits = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            if digits < 3 {
                millis = millis * 10 + i64::from(b[i] - b'0');
                digits += 1;
            }
            i += 1;
        }
        if i == start {
            return None; // un punto sin dígitos no es válido
        }
        while digits < 3 {
            millis *= 10; // escala a milisegundos (`.5` ⇒ 500)
            digits += 1;
        }
    }

    // Zona horaria, obligatoria en RFC 3339.
    let offset_min: i64 = match b.get(i) {
        Some(b'Z' | b'z') => {
            i += 1;
            0
        }
        Some(c @ (b'+' | b'-')) => {
            let sign = if *c == b'+' { 1 } else { -1 };
            i += 1;
            if i + 5 > b.len() || b[i + 2] != b':' {
                return None;
            }
            let oh = parse_n(&b[i..i + 2])?;
            let om = parse_n(&b[i + 3..i + 5])?;
            if oh > 23 || om > 59 {
                return None;
            }
            i += 5;
            sign * (oh * 60 + om)
        }
        _ => return None,
    };
    if i != b.len() {
        return None; // basura al final
    }

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec - offset_min * 60;
    let ms = secs * 1_000 + millis;
    Some(ms.max(0) as u64)
}

/// Entero decimal de un campo de ancho fijo; `None` si hay algún no-dígito.
fn parse_n(b: &[u8]) -> Option<i64> {
    let mut n: i64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + i64::from(c - b'0');
    }
    Some(n)
}

/// Días desde 1970-01-01 para una fecha del calendario gregoriano proléptico
/// (algoritmo de Howard Hinnant, sin tablas ni `unsafe`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_instants() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        // 2026-05-01T00:00:00Z = 1 745 + ... verificado contra epoch conocido.
        assert_eq!(
            parse_rfc3339_ms("2026-05-01T00:00:00Z"),
            Some(1_777_593_600_000)
        );
        // Un segundo después.
        assert_eq!(
            parse_rfc3339_ms("2026-05-01T00:00:01Z"),
            Some(1_777_593_601_000)
        );
    }

    #[test]
    fn fraction_truncates_to_millis() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.250Z"), Some(250));
        // Más de 3 dígitos: se trunca a ms, no redondea.
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.123999Z"), Some(123));
    }

    #[test]
    fn timezone_offsets_apply() {
        // 01:00+01:00 == 00:00Z.
        assert_eq!(
            parse_rfc3339_ms("1970-01-01T01:00:00+01:00"),
            parse_rfc3339_ms("1970-01-01T00:00:00Z"),
        );
        // -05:00 está 5 h por detrás de UTC.
        assert_eq!(
            parse_rfc3339_ms("1970-01-01T00:00:00-05:00"),
            Some(5 * 3_600 * 1_000),
        );
    }

    #[test]
    fn separator_variants() {
        let z = parse_rfc3339_ms("2026-05-01T12:30:00Z");
        assert_eq!(parse_rfc3339_ms("2026-05-01t12:30:00Z"), z);
        assert_eq!(parse_rfc3339_ms("2026-05-01 12:30:00Z"), z);
    }

    #[test]
    fn before_epoch_saturates_to_zero() {
        assert_eq!(parse_rfc3339_ms("1969-12-31T23:59:59Z"), Some(0));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_rfc3339_ms("2026-05-01"), None); // sin hora ni zona
        assert_eq!(parse_rfc3339_ms("2026-05-01T00:00:00"), None); // sin zona
        assert_eq!(parse_rfc3339_ms("2026-13-01T00:00:00Z"), None); // mes inválido
        assert_eq!(parse_rfc3339_ms("2026-05-32T00:00:00Z"), None); // día inválido
        assert_eq!(parse_rfc3339_ms("2026-05-01T24:00:00Z"), None); // hora inválida
        assert_eq!(parse_rfc3339_ms("2026-05-01T00:00:00.Z"), None); // fracción vacía
        assert_eq!(parse_rfc3339_ms("2026-05-01T00:00:00Z "), None); // basura final
        assert_eq!(parse_rfc3339_ms("not-a-date-at-all!!"), None);
    }
}
