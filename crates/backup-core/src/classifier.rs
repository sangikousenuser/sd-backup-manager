//! Photos / Videos / Other への分類（FR-009〜FR-014、4 章）。
//!
//! 拡張子は「分類用」にのみ使い、未知の拡張子は必ず Other に保存する。
//! 付随ファイル（XML/THM/XMP 等）は同名の写真・動画や既知の収録構造に合わせて
//! グループ化し、判断できない場合は Other へ退避する。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// リポジトリ同梱の既定定義。
pub const DEFAULT_FORMATS_TOML: &str = include_str!("../../../config/formats.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Photo,
    Video,
    Other,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Photo => "photo",
            MediaKind::Video => "video",
            MediaKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "photo" => MediaKind::Photo,
            "video" => MediaKind::Video,
            "other" => MediaKind::Other,
            _ => return None,
        })
    }

    /// 格納先のトップフォルダ名。
    pub fn folder(self) -> &'static str {
        match self {
            MediaKind::Photo => "Photos",
            MediaKind::Video => "Videos",
            MediaKind::Other => "Other",
        }
    }
}

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("形式定義の読み込みに失敗しました: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("未対応の形式定義スキーマです: {0}")]
    Schema(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatDefinitions {
    pub schema_version: u32,
    #[serde(default)]
    pub photo: Vec<String>,
    #[serde(default)]
    pub raw_photo: Vec<String>,
    #[serde(default)]
    pub video: Vec<String>,
    #[serde(default)]
    pub raw_video: Vec<String>,
    #[serde(default)]
    pub sidecar: Vec<String>,
    #[serde(default)]
    pub ambiguous_dng: Vec<String>,
    #[serde(default)]
    pub ambiguous_audio: Vec<String>,
    #[serde(default = "default_dng_seq")]
    pub cinema_dng_min_sequence: usize,
    #[serde(default)]
    pub video_structures: Vec<String>,
    #[serde(default)]
    pub video_directory_suffixes: Vec<String>,
}

fn default_dng_seq() -> usize {
    24
}

impl Default for FormatDefinitions {
    fn default() -> Self {
        Self::parse(DEFAULT_FORMATS_TOML).expect("bundled formats.toml must be valid")
    }
}

impl FormatDefinitions {
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let defs: FormatDefinitions = toml::from_str(text)?;
        if defs.schema_version != 1 {
            return Err(FormatError::Schema(defs.schema_version));
        }
        Ok(defs)
    }

    fn sets(&self) -> Sets {
        let lower = |v: &[String]| -> HashSet<String> {
            v.iter()
                .map(|s| s.trim().trim_start_matches('.').to_lowercase())
                .collect()
        };
        let mut photo = lower(&self.photo);
        photo.extend(lower(&self.raw_photo));
        let mut video = lower(&self.video);
        video.extend(lower(&self.raw_video));
        Sets {
            photo,
            video,
            sidecar: lower(&self.sidecar),
            dng: lower(&self.ambiguous_dng),
            audio: lower(&self.ambiguous_audio),
            structures: self
                .video_structures
                .iter()
                .map(|s| {
                    s.trim_matches('/')
                        .split('/')
                        .map(|c| c.to_lowercase())
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty() && !v[0].is_empty())
                .collect(),
            dir_suffixes: lower(&self.video_directory_suffixes),
        }
    }
}

struct Sets {
    photo: HashSet<String>,
    video: HashSet<String>,
    sidecar: HashSet<String>,
    dng: HashSet<String>,
    audio: HashSet<String>,
    structures: Vec<Vec<String>>,
    dir_suffixes: HashSet<String>,
}

/// 分類器への入力（1 パーティション内の 1 ファイル）。
#[derive(Debug, Clone)]
pub struct ClassifyInput {
    /// パーティションルートからの相対パス要素。
    pub components: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub kind: MediaKind,
    /// 関連ファイル推定情報（マニフェスト用）。例: `structure:PRIVATE/M4ROOT`, `stem:DCIM/100/DSC0001`
    pub group: Option<String>,
    /// 分類の根拠（診断用）。
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Primary {
    Photo,
    Video,
    Sidecar,
    Dng,
    Audio,
    Unknown,
}

fn extension(name: &str) -> Option<String> {
    let pos = name.rfind('.')?;
    if pos == 0 || pos + 1 == name.len() {
        return None;
    }
    Some(name[pos + 1..].to_lowercase())
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(pos) if pos > 0 => &name[..pos],
        _ => name,
    }
}

/// 1 パーティション分のファイル群をまとめて分類する（入力と同じ順で返す）。
pub fn classify_partition(
    defs: &FormatDefinitions,
    files: &[ClassifyInput],
) -> Vec<Classification> {
    let sets = defs.sets();

    let primaries: Vec<Primary> = files
        .iter()
        .map(|f| {
            let name = f.components.last().map(String::as_str).unwrap_or("");
            match extension(name) {
                Some(ext) if sets.photo.contains(&ext) => Primary::Photo,
                Some(ext) if sets.video.contains(&ext) => Primary::Video,
                Some(ext) if sets.dng.contains(&ext) => Primary::Dng,
                Some(ext) if sets.audio.contains(&ext) => Primary::Audio,
                Some(ext) if sets.sidecar.contains(&ext) => Primary::Sidecar,
                _ => Primary::Unknown,
            }
        })
        .collect();

    // ディレクトリごとの DNG 数（CinemaDNG 判定）
    let mut dng_per_dir: HashMap<String, usize> = HashMap::new();
    for (f, p) in files.iter().zip(&primaries) {
        if *p == Primary::Dng {
            *dng_per_dir.entry(dir_key(&f.components)).or_default() += 1;
        }
    }

    // 1 段階目: 主ファイルの確定
    let mut result: Vec<Option<Classification>> = Vec::with_capacity(files.len());
    for (f, p) in files.iter().zip(&primaries) {
        if let Some(structure) = video_structure(&sets, &f.components) {
            result.push(Some(Classification {
                kind: MediaKind::Video,
                group: Some(format!("structure:{structure}")),
                reason: "known_video_structure",
            }));
            continue;
        }
        let c = match p {
            Primary::Photo => Some(Classification {
                kind: MediaKind::Photo,
                group: Some(stem_group(&f.components)),
                reason: "photo_extension",
            }),
            Primary::Video => Some(Classification {
                kind: MediaKind::Video,
                group: Some(stem_group(&f.components)),
                reason: "video_extension",
            }),
            Primary::Dng => {
                let count = dng_per_dir
                    .get(&dir_key(&f.components))
                    .copied()
                    .unwrap_or(0);
                if count >= defs.cinema_dng_min_sequence {
                    Some(Classification {
                        kind: MediaKind::Video,
                        group: Some(format!("cinemadng:{}", dir_key(&f.components))),
                        reason: "cinema_dng_sequence",
                    })
                } else {
                    Some(Classification {
                        kind: MediaKind::Photo,
                        group: Some(stem_group(&f.components)),
                        reason: "dng_still",
                    })
                }
            }
            _ => None,
        };
        result.push(c);
    }

    // 主ファイルの (dir, stem) 索引
    let mut by_stem: HashMap<(String, String), MediaKind> = HashMap::new();
    for (f, c) in files.iter().zip(&result) {
        if let Some(c) = c {
            if c.kind == MediaKind::Other {
                continue;
            }
            let key = (
                dir_key(&f.components),
                stem(f.components.last().unwrap()).to_lowercase(),
            );
            // 動画が優先（同名の JPG サムネイル + MP4 等）
            by_stem
                .entry(key)
                .and_modify(|k| {
                    if c.kind == MediaKind::Video {
                        *k = MediaKind::Video
                    }
                })
                .or_insert(c.kind);
        }
    }
    let mut primaries_per_dir: HashMap<String, Vec<(String, MediaKind)>> = HashMap::new();
    for ((dir, st), kind) in &by_stem {
        primaries_per_dir
            .entry(dir.clone())
            .or_default()
            .push((st.clone(), *kind));
    }

    // 2 段階目: 付随ファイル・音声・未知形式
    files
        .iter()
        .zip(primaries)
        .zip(result)
        .map(|((f, p), c)| {
            if let Some(c) = c {
                return c;
            }
            let name = f.components.last().map(String::as_str).unwrap_or("");
            let dir = dir_key(&f.components);
            let my_stem = stem(name).to_lowercase();
            let grouped = match_primary(&primaries_per_dir, &dir, &my_stem);
            match (p, grouped) {
                (
                    Primary::Sidecar | Primary::Audio | Primary::Unknown,
                    Some((kind, primary_stem)),
                ) => Classification {
                    kind,
                    group: Some(format!("stem:{}", join_group(&dir, &primary_stem))),
                    reason: match p {
                        Primary::Sidecar => "sidecar_matched",
                        Primary::Audio => "audio_matched",
                        _ => "unknown_matched_stem",
                    },
                },
                (Primary::Audio, None) => Classification {
                    kind: MediaKind::Other,
                    group: None,
                    reason: "standalone_audio",
                },
                (Primary::Sidecar, None) => Classification {
                    kind: MediaKind::Other,
                    group: None,
                    reason: "unmatched_sidecar",
                },
                _ => Classification {
                    kind: MediaKind::Other,
                    group: None,
                    reason: "unknown_extension",
                },
            }
        })
        .collect()
}

/// 同じディレクトリの主ファイルで、stem が一致するか前方一致（例: `C0001M01.XML` → `C0001`）するもの。
fn match_primary(
    primaries_per_dir: &HashMap<String, Vec<(String, MediaKind)>>,
    dir: &str,
    my_stem: &str,
) -> Option<(MediaKind, String)> {
    let candidates = primaries_per_dir.get(dir)?;
    // 完全一致を優先
    if let Some((s, k)) = candidates.iter().find(|(s, _)| s == my_stem) {
        return Some((*k, s.clone()));
    }
    // `DSC0001.JPG.xmp` のような二重拡張子
    let inner = stem(my_stem);
    if inner != my_stem
        && let Some((s, k)) = candidates.iter().find(|(s, _)| s == inner)
    {
        return Some((*k, s.clone()));
    }
    // 前方一致（最長一致、4 文字以上）
    candidates
        .iter()
        .filter(|(s, _)| s.len() >= 4 && my_stem.starts_with(s.as_str()))
        .max_by_key(|(s, _)| s.len())
        .map(|(s, k)| (*k, s.clone()))
}

fn video_structure(sets: &Sets, components: &[String]) -> Option<String> {
    let dirs = &components[..components.len().saturating_sub(1)];
    let lower: Vec<String> = dirs.iter().map(|c| c.to_lowercase()).collect();
    for s in &sets.structures {
        if lower.len() >= s.len() && lower[..s.len()] == s[..] {
            return Some(dirs[..s.len()].join("/"));
        }
    }
    for (i, d) in lower.iter().enumerate() {
        if let Some(ext) = extension(d)
            && sets.dir_suffixes.contains(&ext)
        {
            return Some(dirs[..=i].join("/"));
        }
    }
    None
}

fn dir_key(components: &[String]) -> String {
    components[..components.len().saturating_sub(1)]
        .iter()
        .map(|c| c.to_lowercase())
        .collect::<Vec<_>>()
        .join("/")
}

fn stem_group(components: &[String]) -> String {
    let name = components.last().map(String::as_str).unwrap_or("");
    format!(
        "stem:{}",
        join_group(&dir_key(components), &stem(name).to_lowercase())
    )
}

fn join_group(dir: &str, stem: &str) -> String {
    if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str) -> ClassifyInput {
        ClassifyInput {
            components: path.split('/').map(str::to_string).collect(),
        }
    }

    fn classify(paths: &[&str]) -> Vec<MediaKind> {
        let defs = FormatDefinitions::default();
        let inputs: Vec<_> = paths.iter().map(|p| input(p)).collect();
        classify_partition(&defs, &inputs)
            .into_iter()
            .map(|c| c.kind)
            .collect()
    }

    #[test]
    fn bundled_definitions_parse() {
        let defs = FormatDefinitions::default();
        assert!(defs.photo.contains(&"jpg".to_string()));
        assert!(defs.raw_photo.contains(&"arw".to_string()));
        assert!(defs.video.contains(&"mp4".to_string()));
    }

    #[test]
    fn sony_card_layout() {
        let kinds = classify(&[
            "DCIM/100MSDCF/DSC0001.ARW",
            "DCIM/100MSDCF/DSC0001.JPG",
            "PRIVATE/M4ROOT/CLIP/C0001.MP4",
            "PRIVATE/M4ROOT/CLIP/C0001M01.XML",
            "PRIVATE/M4ROOT/THMBNL/C0001T01.JPG",
            "PRIVATE/M4ROOT/MEDIAPRO.XML",
            "PRIVATE/M4ROOT/STATUS.BIN",
            "README.TXT",
        ]);
        use MediaKind::*;
        assert_eq!(
            kinds,
            vec![Photo, Photo, Video, Video, Video, Video, Video, Other]
        );
    }

    #[test]
    fn sidecars_follow_their_primary() {
        let kinds = classify(&[
            "DCIM/100GOPRO/GX010001.MP4",
            "DCIM/100GOPRO/GX010001.THM",
            "DCIM/100GOPRO/GL010001.LRV",
            "DCIM/101/IMG_0001.CR3",
            "DCIM/101/IMG_0001.xmp",
            "DCIM/101/IMG_0002.JPG.xmp",
            "DCIM/101/IMG_0002.JPG",
            "DJI/DJI_0001.MP4",
            "DJI/DJI_0001.SRT",
            "orphan/meta.xml",
        ]);
        use MediaKind::*;
        // GL010001.LRV は stem が違うので Other（安全側）
        assert_eq!(
            kinds,
            vec![
                Video, Video, Other, Photo, Photo, Photo, Photo, Video, Video, Other
            ]
        );
    }

    #[test]
    fn wav_is_grouped_or_other() {
        let kinds = classify(&["CLIPS/A001.MOV", "CLIPS/A001.WAV", "AUDIO/ZOOM0001.WAV"]);
        assert_eq!(
            kinds,
            vec![MediaKind::Video, MediaKind::Video, MediaKind::Other]
        );
    }

    #[test]
    fn cinema_dng_sequence_vs_still() {
        let mut paths: Vec<String> = (0..30)
            .map(|i| format!("CLIP_A/frame_{i:06}.dng"))
            .collect();
        paths.push("DCIM/100/IMG_0001.DNG".into());
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let kinds = classify(&refs);
        assert!(kinds[..30].iter().all(|k| *k == MediaKind::Video));
        assert_eq!(kinds[30], MediaKind::Photo);
    }

    #[test]
    fn red_and_avchd_structures() {
        let kinds = classify(&[
            "A001_C001.RDC/A001_C001_0101AB_001.R3D",
            "A001_C001.RDC/A001_C001_0101AB.RTN",
            "PRIVATE/AVCHD/BDMV/STREAM/00000.MTS",
            "PRIVATE/AVCHD/BDMV/CLIPINF/00000.CPI",
            "PRIVATE/AVCHD/BDMV/INDEX.BDM",
        ]);
        assert!(kinds.iter().all(|k| *k == MediaKind::Video));
    }

    #[test]
    fn unknown_extensions_go_to_other_never_dropped() {
        let kinds = classify(&["weird.xyz", "noext", ".hidden", "Thumbs.db"]);
        assert!(kinds.iter().all(|k| *k == MediaKind::Other));
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn custom_definitions() {
        let defs = FormatDefinitions::parse(
            "schema_version = 1\nphoto = [\"foo\"]\nvideo_structures = [\"MYCAM\"]\n",
        )
        .unwrap();
        let out = classify_partition(
            &defs,
            &[input("x.FOO"), input("mycam/a.bin"), input("y.jpg")],
        );
        assert_eq!(out[0].kind, MediaKind::Photo);
        assert_eq!(out[1].kind, MediaKind::Video);
        assert_eq!(out[2].kind, MediaKind::Other);
        assert!(FormatDefinitions::parse("schema_version = 2").is_err());
    }
}
