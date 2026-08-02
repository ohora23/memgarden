use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const APP_DIR: &str = "memgarden";
const DB_FILE: &str = "memgarden.db";
const CONFIG_FILE: &str = "config.toml";
const MODELS_SUBDIR: &str = "models";

/// Pure resolution of the data directory from explicit env values.
/// `$XDG_DATA_HOME/memgarden` if set (non-empty), else `$HOME/.local/share/memgarden`.
pub fn data_dir_from(xdg_data_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(xdg) = non_empty(xdg_data_home) {
        return Ok(PathBuf::from(xdg).join(APP_DIR));
    }
    let home = non_empty(home)
        .ok_or_else(|| Error::Config("neither XDG_DATA_HOME nor HOME is set".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join(APP_DIR))
}

/// Pure resolution of the config file path from explicit env values.
/// `$XDG_CONFIG_HOME/memgarden/config.toml` if set (non-empty), else
/// `$HOME/.config/memgarden/config.toml`.
pub fn config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(xdg) = non_empty(xdg_config_home) {
        return Ok(PathBuf::from(xdg).join(APP_DIR).join(CONFIG_FILE));
    }
    let home = non_empty(home)
        .ok_or_else(|| Error::Config("neither XDG_CONFIG_HOME nor HOME is set".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join(APP_DIR)
        .join(CONFIG_FILE))
}

/// Pure resolution of the default sqlite db path: `<data_dir>/memgarden.db`.
pub fn default_db_path_from(xdg_data_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    Ok(data_dir_from(xdg_data_home, home)?.join(DB_FILE))
}

/// Pure resolution of the embedding-model cache dir: `<data_dir>/models`.
/// Passed to `fastembed::InitOptions::with_cache_dir` and (CE-11)
/// `hf_hub::api::sync::ApiBuilder::with_cache_dir` — offline-friendly, since
/// fastembed's own default (`./.fastembed_cache`, relative to CWD) is
/// unusable for a daemon (plan decision #1 / Verified Environment Facts).
pub fn models_dir_from(xdg_data_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    Ok(data_dir_from(xdg_data_home, home)?.join(MODELS_SUBDIR))
}

fn non_empty(v: Option<&str>) -> Option<&str> {
    v.filter(|s| !s.is_empty())
}

/// Thin wrapper reading the real process environment.
pub fn data_dir() -> Result<PathBuf> {
    data_dir_from(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Thin wrapper reading the real process environment.
pub fn config_path() -> Result<PathBuf> {
    config_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Thin wrapper reading the real process environment.
pub fn default_db_path() -> Result<PathBuf> {
    default_db_path_from(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Thin wrapper reading the real process environment.
pub fn models_dir() -> Result<PathBuf> {
    models_dir_from(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Create `dir` (and parents) if missing, ensuring mode 0700 on unix.
pub fn ensure_data_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| Error::Io {
                path: dir.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_prefers_xdg_data_home() {
        let p = data_dir_from(Some("/x/data"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/x/data/memgarden"));
    }

    #[test]
    fn data_dir_falls_back_to_home() {
        let p = data_dir_from(None, Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/share/memgarden"));
    }

    #[test]
    fn data_dir_treats_empty_xdg_as_unset() {
        let p = data_dir_from(Some(""), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/share/memgarden"));
    }

    #[test]
    fn data_dir_errors_without_any_env() {
        assert!(data_dir_from(None, None).is_err());
    }

    #[test]
    fn config_path_prefers_xdg_config_home() {
        let p = config_path_from(Some("/x/config"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/x/config/memgarden/config.toml"));
    }

    #[test]
    fn config_path_falls_back_to_home() {
        let p = config_path_from(None, Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/memgarden/config.toml"));
    }

    #[test]
    fn default_db_path_is_data_dir_plus_file() {
        let p = default_db_path_from(Some("/x/data"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/x/data/memgarden/memgarden.db"));
    }

    #[test]
    fn models_dir_is_data_dir_plus_subdir() {
        let p = models_dir_from(Some("/x/data"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/x/data/memgarden/models"));
    }

    #[test]
    fn ensure_data_dir_creates_with_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested").join("memgarden");
        ensure_data_dir(&target).unwrap();
        assert!(target.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
