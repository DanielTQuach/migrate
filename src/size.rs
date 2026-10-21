//! Approximate size of a folder from the Windows search index.
//!
//! The index already stores a size for each file it knows about. Asking it
//! avoids opening every nested folder. The number can be low when Windows has
//! not indexed a file, so the window shows it with a "~".

use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x08000000;

/// One indexed total per path, in the same order. `None` means the index had
/// nothing for that folder.
pub fn indexed_sizes(paths: &[impl AsRef<Path>]) -> Vec<Option<u64>> {
    if paths.is_empty() {
        return Vec::new();
    }
    match query(paths) {
        Ok(sizes) if sizes.len() == paths.len() => sizes,
        _ => vec![None; paths.len()],
    }
}

fn query(paths: &[impl AsRef<Path>]) -> io::Result<Vec<Option<u64>>> {
    let script = size_script(paths);
    let encoded = encode_utf16(&script);
    let output = Command::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-OutputFormat",
            "Text",
            "-EncodedCommand",
            &encoded,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().map(parse_size_line).collect())
}

fn size_script(paths: &[impl AsRef<Path>]) -> String {
    let mut listed = String::new();
    for path in paths {
        let escaped = path.as_ref().display().to_string().replace('\'', "''");
        listed.push_str("  '");
        listed.push_str(&escaped);
        listed.push_str("'\n");
    }
    format!(
        r#"
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'SilentlyContinue'
$paths = @(
{listed})
$conn = New-Object -ComObject ADODB.Connection
$opened = $false
try {{
  $conn.Open("Provider=Search.CollatorDSO;Extended Properties='Application=Windows';")
  $opened = $true
}} catch {{}}
foreach ($path in $paths) {{
  if (-not $opened) {{ Write-Output ''; continue }}
  $safe = ([string]$path).Replace("'", "''")
  $sql = "SELECT System.Size FROM SystemIndex WHERE SCOPE='file:$safe'"
  $total = [uint64]0
  $any = $false
  try {{
    $rs = $conn.Execute($sql)
    while (-not $rs.EOF) {{
      $value = $rs.Fields.Item(0).Value
      if ($null -ne $value -and $value -isnot [DBNull]) {{
        $total += [uint64]$value
        $any = $true
      }}
      $rs.MoveNext()
    }}
    $rs.Close()
  }} catch {{}}
  if ($any) {{ Write-Output $total }} else {{ Write-Output '' }}
}}
if ($opened) {{ $conn.Close() }}
"#
    )
}

fn encode_utf16(script: &str) -> String {
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64_encode(&bytes)
}

fn parse_size_line(line: &str) -> Option<u64> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    line.parse().ok()
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let chunk = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | bytes[index + 2] as u32;
        out.push(TABLE[((chunk >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((chunk >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((chunk >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(chunk & 0x3F) as usize] as char);
        index += 3;
    }
    let rest = bytes.len() - index;
    if rest == 1 {
        let chunk = (bytes[index] as u32) << 16;
        out.push(TABLE[((chunk >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((chunk >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let chunk = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8);
        out.push(TABLE[((chunk >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((chunk >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((chunk >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_line_means_no_indexed_size() {
        assert_eq!(parse_size_line(""), None);
        assert_eq!(parse_size_line("   "), None);
        assert_eq!(parse_size_line("1037914266"), Some(1_037_914_266));
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_encode(b"h"), "aA==");
    }
}
