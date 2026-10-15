//! 日本語フォントの検出。Iced は既定でシステムフォントを読み込まないため、
//! CJK フォントのファイルを探して登録し、既定フォントに設定する。

const CANDIDATES: &[(&str, &str)] = &[
    (
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "Noto Sans CJK JP",
    ),
    (
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "Noto Sans CJK JP",
    ),
    (
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "Noto Sans CJK JP",
    ),
    (
        "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf",
        "Noto Sans CJK JP",
    ),
    (
        "/usr/share/fonts/truetype/fonts-japanese-gothic.ttf",
        "IPAexGothic",
    ),
    (
        "/usr/share/fonts/opentype/ipaexfont-gothic/ipaexg.ttf",
        "IPAexGothic",
    ),
    (
        "/usr/share/fonts/truetype/takao-gothic/TakaoPGothic.ttf",
        "TakaoPGothic",
    ),
    (
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "Hiragino Sans",
    ),
    (
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "Hiragino Sans GB",
    ),
];

pub fn find_japanese_font() -> Option<(Vec<u8>, &'static str)> {
    for (path, family) in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            return Some((bytes, family));
        }
    }
    // fontconfig に問い合わせる
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n%{family[0]}", ":lang=ja"])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let file = lines.next()?.trim().to_string();
    let family = lines.next()?.trim().to_string();
    let bytes = std::fs::read(&file).ok()?;
    Some((bytes, Box::leak(family.into_boxed_str())))
}
