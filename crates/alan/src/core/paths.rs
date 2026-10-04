use anyhow::Result;
use std::path::PathBuf;

/// The Alan home directory (`$ALAN_HOME`, or `$HOME`).
///
/// `ALAN_HOME` exists so everything Alan writes can be redirected somewhere
/// else, which makes it the home for every dot-directory Alan reads, not just
/// the data directory.
pub fn alan_home() -> Result<PathBuf> {
    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine Alan home directory"))?;

    Ok(PathBuf::from(home))
}

/// The Alan data directory (`$ALAN_HOME/.alan`, or `$HOME/.alan`).
pub fn alan_data_dir() -> Result<PathBuf> {
    Ok(alan_home()?.join(".alan"))
}

/// A path to a file inside the data directory.
pub fn data_file(name: &str) -> Result<PathBuf> {
    Ok(alan_data_dir()?.join(name))
}
