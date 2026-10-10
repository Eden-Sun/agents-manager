//! 分享頁上傳用的 `multipart/form-data`：只取一個名為 `file` 的欄位（瀏覽器 `FormData.append('file', f)` 送的形狀）。
//! 沒有拉 form 解析的相依（離線也編得過）：只認標準形狀，任何不合的地方一律拒絕，不猜。

/// `(檔名, 內容)`。檔名是呼叫端給的原字串，還要交給 `clean_upload_name` 清。
pub fn file_part<'a>(content_type: &str, body: &'a [u8]) -> Result<(String, &'a [u8]), &'static str> {
    let boundary = boundary(content_type).ok_or("bad_multipart")?;
    let delim = [b"--".as_slice(), boundary.as_bytes()].concat();
    let mut at = find(body, &delim, 0).ok_or("bad_multipart")? + delim.len();
    let mut found: Option<(String, &[u8])> = None;
    loop {
        let rest = body.get(at..).ok_or("bad_multipart")?;
        if rest.starts_with(b"--") {
            break; // 結尾的 `--boundary--`
        }
        if !rest.starts_with(b"\r\n") {
            return Err("bad_multipart");
        }
        let head_start = at + 2;
        let head_end = find(body, b"\r\n\r\n", head_start).ok_or("bad_multipart")?;
        let headers = std::str::from_utf8(&body[head_start..head_end]).map_err(|_| "bad_multipart")?;
        let content_start = head_end + 4;
        let next = find(body, &[b"\r\n".as_slice(), &delim].concat(), content_start).ok_or("bad_multipart")?;
        let content = &body[content_start..next];
        if let Some((name, filename)) = disposition(headers) {
            if name == "file" {
                if found.is_some() {
                    return Err("bad_multipart"); // 一次只收一個檔
                }
                found = Some((filename.ok_or("bad_multipart")?, content));
            }
        }
        at = next + 2 + delim.len();
    }
    found.ok_or("no_file")
}

fn boundary(content_type: &str) -> Option<String> {
    let (mime, params) = content_type.split_once(';')?;
    if !mime.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    let b = params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
    })?;
    (!b.is_empty() && b.len() <= 70 && b.bytes().all(|c| c.is_ascii_graphic() || c == b' ')).then_some(b)
}

/// `Content-Disposition: form-data; name="file"; filename="a.txt"` → `("file", Some("a.txt"))`。
fn disposition(headers: &str) -> Option<(String, Option<String>)> {
    let line = headers.split("\r\n").find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))?;
    let value = &line["content-disposition:".len()..];
    let mut parts = split_params(value).into_iter();
    if !parts.next()?.trim().eq_ignore_ascii_case("form-data") {
        return None;
    }
    let (mut name, mut filename) = (None, None);
    for p in parts {
        let Some((k, v)) = p.split_once('=') else { continue };
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v).replace("%22", "\"");
        match k.trim().to_ascii_lowercase().as_str() {
            "name" => name = Some(v),
            "filename" => filename = Some(v),
            _ => {}
        }
    }
    Some((name?, filename))
}

/// 以 `;` 切參數；雙引號裡的 `;` 不算（瀏覽器不跳脫檔名裡的 `;`，只把 `"` 寫成 `%22`）。
fn split_params(s: &str) -> Vec<&str> {
    let (mut out, mut start, mut quoted) = (Vec::new(), 0, false);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, filename, data) in parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(f) => out.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes()),
                None => out.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes()),
            }
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    #[test]
    fn the_file_field_comes_out_byte_for_byte() {
        let data = b"line1\r\n--not-the-boundary\r\nline3\0\xff";
        let body = form("----WebKitFormBoundaryX1", &[("note", None, b"hi"), ("file", Some("報表.csv"), data)]);
        let (name, got) = file_part("multipart/form-data; boundary=----WebKitFormBoundaryX1", &body).unwrap();
        assert_eq!(name, "報表.csv");
        assert_eq!(got, data);
        let quoted = file_part("multipart/form-data; boundary=\"----WebKitFormBoundaryX1\"", &body).unwrap();
        assert_eq!(quoted.1, data);
    }

    #[test]
    fn anything_off_shape_is_refused() {
        let body = form("B", &[("file", Some("a.txt"), b"x")]);
        assert_eq!(file_part("application/octet-stream", &body), Err("bad_multipart"));
        assert_eq!(file_part("multipart/form-data", &body), Err("bad_multipart"), "沒有 boundary");
        assert_eq!(file_part("multipart/form-data; boundary=C", &body), Err("bad_multipart"));
        assert_eq!(file_part("multipart/form-data; boundary=B", &body[..body.len() - 12]), Err("bad_multipart"), "截斷");
        let none = form("B", &[("other", Some("a.txt"), b"x")]);
        assert_eq!(file_part("multipart/form-data; boundary=B", &none), Err("no_file"));
        let two = form("B", &[("file", Some("a.txt"), b"x"), ("file", Some("b.txt"), b"y")]);
        assert_eq!(file_part("multipart/form-data; boundary=B", &two), Err("bad_multipart"), "一次一個檔");
        let nameless = form("B", &[("file", None, b"x")]);
        assert_eq!(file_part("multipart/form-data; boundary=B", &nameless), Err("bad_multipart"));
    }

    /// issue #1164：瀏覽器不跳脫檔名裡的 `;`、`=`（只把 `"` 寫成 `%22`），引號裡的 `;` 不能當參數分隔。
    #[test]
    fn a_filename_with_a_semicolon_or_an_equals_sign_survives() {
        let body = form("B", &[("file", Some("報價;v2=final.pdf"), b"%PDF-1.7")]);
        let (name, got) = file_part("multipart/form-data; boundary=B", &body).unwrap();
        assert_eq!(name, "報價;v2=final.pdf");
        assert_eq!(got, b"%PDF-1.7");
        let body = form("B", &[("note", None, b"hi"), ("file", Some("a;b.png"), b"png")]);
        let (name, _) = file_part("multipart/form-data; boundary=B", &body).unwrap();
        assert_eq!(name, "a;b.png");
    }
}
