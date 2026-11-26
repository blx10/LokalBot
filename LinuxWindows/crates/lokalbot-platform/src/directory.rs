use std::collections::HashSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdPerson {
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinguished_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_principal_name: Option<String>,
}

pub fn escape_ldap_filter(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str(r"\5c"),
            '*' => out.push_str(r"\2a"),
            '(' => out.push_str(r"\28"),
            ')' => out.push_str(r"\29"),
            '\0' => out.push_str(r"\00"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn build_ldap_filter(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.chars().count() < 3 {
        return None;
    }
    let escaped = escape_ldap_filter(trimmed);
    Some(format!(
        "(&(objectCategory=person)(objectClass=user)(displayName=*{escaped}*))"
    ))
}

pub fn parse_ad_search_json(raw: &str) -> Vec<AdPerson> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let items: Vec<AdPerson> = match serde_json::from_str::<Vec<AdPerson>>(trimmed) {
        Ok(list) => list,
        Err(_) => match serde_json::from_str::<AdPerson>(trimmed) {
            Ok(single) => vec![single],
            Err(_) => return Vec::new(),
        },
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for mut item in items {
        item.display_name = item.display_name.trim().to_string();
        if item.display_name.is_empty() {
            continue;
        }
        item.object_guid = item
            .object_guid
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        item.distinguished_name = item
            .distinguished_name
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        item.user_principal_name = item
            .user_principal_name
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let dedup_key = item
            .object_guid
            .clone()
            .unwrap_or_else(|| item.display_name.to_lowercase());
        if seen.insert(dedup_key) {
            out.push(item);
        }
    }
    out
}

#[cfg(windows)]
pub fn is_ad_joined() -> bool {
    std::env::var_os("USERDNSDOMAIN").is_some_and(|v| !v.is_empty())
}

#[cfg(not(windows))]
pub fn is_ad_joined() -> bool {
    false
}

#[cfg(windows)]
pub fn search_ad_people(query: &str, limit: usize) -> Vec<AdPerson> {
    let Some(filter) = build_ldap_filter(query) else {
        return Vec::new();
    };
    if !is_ad_joined() || limit == 0 {
        return Vec::new();
    }
    let limit = limit.min(25);
    let ps_filter = filter.replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop'; \
$s = New-Object System.DirectoryServices.DirectorySearcher; \
$s.Filter = '{ps_filter}'; \
$s.PageSize = {limit}; \
$s.SizeLimit = {limit}; \
$null = $s.PropertiesToLoad.Add('displayName'); \
$null = $s.PropertiesToLoad.Add('objectGUID'); \
$null = $s.PropertiesToLoad.Add('distinguishedName'); \
$null = $s.PropertiesToLoad.Add('userPrincipalName'); \
$rows = foreach ($r in $s.FindAll()) {{ \
  $p = $r.Properties; \
  $dn = if ($p['displayname'].Count -gt 0) {{ [string]$p['displayname'][0] }} else {{ '' }}; \
  if ([string]::IsNullOrWhiteSpace($dn)) {{ continue }}; \
  $guid = if ($p['objectguid'].Count -gt 0) {{ ([guid][byte[]]$p['objectguid'][0]).ToString() }} else {{ $null }}; \
  $dist = if ($p['distinguishedname'].Count -gt 0) {{ [string]$p['distinguishedname'][0] }} else {{ $null }}; \
  $upn = if ($p['userprincipalname'].Count -gt 0) {{ [string]$p['userprincipalname'][0] }} else {{ $null }}; \
  [pscustomobject]@{{ displayName = $dn; objectGuid = $guid; distinguishedName = $dist; userPrincipalName = $upn }} \
}}; \
$rows | ConvertTo-Json -Compress"
    );
    let Ok(output) = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ad_search_json(&stdout)
        .into_iter()
        .take(limit)
        .collect()
}

#[cfg(not(windows))]
pub fn search_ad_people(_query: &str, _limit: usize) -> Vec<AdPerson> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldap_filter_escapes_special_characters() {
        assert_eq!(escape_ldap_filter(r"A*(B)\C"), r"A\2a\28B\29\5cC");
    }

    #[test]
    fn build_ldap_filter_requires_three_characters() {
        assert_eq!(build_ldap_filter(""), None);
        assert_eq!(build_ldap_filter("  al "), None);
        assert_eq!(
            build_ldap_filter(" alp "),
            Some("(&(objectCategory=person)(objectClass=user)(displayName=*alp*))".into())
        );
    }

    #[test]
    fn parse_ad_search_json_handles_array_single_and_empty() {
        let array = r#"[{"displayName":"Ayse Yilmaz","objectGuid":"1122","distinguishedName":"CN=Ayse","userPrincipalName":"ayse@corp.local"},{"displayName":"  ","objectGuid":"9999"}]"#;
        let parsed = parse_ad_search_json(array);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].display_name, "Ayse Yilmaz");
        assert_eq!(parsed[0].object_guid.as_deref(), Some("1122"));
        assert_eq!(
            parsed[0].user_principal_name.as_deref(),
            Some("ayse@corp.local")
        );

        let single = r#"{"displayName":"Jane Doe","objectGuid":"abcd","distinguishedName":null,"userPrincipalName":"jane.doe@example.local"}"#;
        let parsed_single = parse_ad_search_json(single);
        assert_eq!(parsed_single.len(), 1);
        assert_eq!(parsed_single[0].display_name, "Jane Doe");

        assert!(parse_ad_search_json("").is_empty());
        assert!(parse_ad_search_json("not json").is_empty());
    }
}
