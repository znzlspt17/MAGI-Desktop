// 2026-11-15: 승인 검사는 문자열 경로만 판정하며 실제 파일 시스템을 확인하지 않는다.
pub(crate) fn protected(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    if p.split('/')
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w == [".config", "gcloud"])
    {
        return true;
    }
    p.split('/').any(|s| {
        s == ".env"
            || s.starts_with(".env.")
            || matches!(
                s,
                ".ssh"
                    | ".aws"
                    | ".azure"
                    | ".kube"
                    | ".git-credentials"
                    | ".npmrc"
                    | ".pypirc"
                    | ".netrc"
                    | "credentials.json"
                    | "secrets.json"
                    | "token.json"
            )
            || [".key", ".p12", ".pfx", ".jks", ".keystore"]
                .iter()
                .any(|x| s.ends_with(x))
    })
}

pub(crate) fn safe_relative(path: &str) -> bool {
    if path.is_empty() || path.starts_with(['/', '\\']) {
        return false;
    }
    path.split(['/', '\\']).all(|part| {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        {
            return false;
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) {
            return false;
        }
        for prefix in ["COM", "LPT"] {
            if let Some(n) = stem.strip_prefix(prefix) {
                if matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                ) {
                    return false;
                }
            }
        }
        true
    })
}
