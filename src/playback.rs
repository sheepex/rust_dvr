use url::Url;

fn parse_datetime(input: &str) -> Result<String, &'static str> {
    let bytes = input.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| !matches!(i, 4 | 7 | 10 | 13 | 16) && !b.is_ascii_digit())
    {
        return Err("Use o formato AAAA-MM-DD HH:MM:SS.");
    }
    let number = |start, end| input[start..end].parse::<u32>().unwrap();
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let (hour, minute, second) = (number(11, 13), number(14, 16), number(17, 19));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
        return Err("Data ou hora inválida.");
    }
    Ok(input.replace(['-', ' ', ':'], "_"))
}

pub fn url(base: Url, channel: u32, start: &str, end: &str) -> Result<Url, &'static str> {
    if channel == 0 {
        return Err("Canal inválido.");
    }
    let start = parse_datetime(start)?;
    let end = parse_datetime(end)?;
    if start >= end {
        return Err("O horário final deve ser posterior ao inicial.");
    }
    let mut url = base;
    url.set_username("").map_err(|_| "URL inválida.")?;
    url.set_password(None).map_err(|_| "URL inválida.")?;
    url.set_path("/cam/playback");
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("channel", &channel.to_string())
        .append_pair("starttime", &start)
        .append_pair("endtime", &end);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_playback_url_without_credentials() {
        let base = Url::parse("rtsp://user:secret@example.com:554/").unwrap();
        let url = url(base, 2, "2024-02-29 12:00:00", "2024-03-01 01:02:03").unwrap();
        assert_eq!(url.as_str(), "rtsp://example.com:554/cam/playback?channel=2&starttime=2024_02_29_12_00_00&endtime=2024_03_01_01_02_03");
    }

    #[test]
    fn rejects_invalid_dates_and_intervals() {
        let base = Url::parse("rtsp://example.com/").unwrap();
        for (start, end) in [
            ("2023-02-29 00:00:00", "2023-03-01 00:00:00"),
            ("2024-01-01 25:00:00", "2024-01-02 00:00:00"),
            ("2024-01-01 00:00:00", "2024-01-01 00:00:00"),
            ("2024-01-01 00:00:00", "2023-12-31 23:59:59"),
            ("2024-01-01T00:00:00", "2024-01-02 00:00:00"),
        ] {
            assert!(url(base.clone(), 1, start, end).is_err());
        }
    }
}
