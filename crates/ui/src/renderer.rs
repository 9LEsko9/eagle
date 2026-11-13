// SPDX-License-Identifier: GPL-3.0-or-later
//! Explicit renderer selection shared by desktop and preview entrypoints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Renderer {
    #[default]
    FemtoVgWgpu,
    Software,
}
impl Renderer {
    pub const fn name(self) -> &'static str {
        match self {
            Self::FemtoVgWgpu => "femtovg-wgpu",
            Self::Software => "software",
        }
    }
    pub fn from_args(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self, String> {
        let mut selected = None;
        for arg in args {
            let arg = arg.as_ref();
            let renderer = if arg == "--software" {
                Self::Software
            } else if let Some(name) = arg.strip_prefix("--renderer=") {
                match name {
                    "femtovg-wgpu" => Self::FemtoVgWgpu,
                    "software" => Self::Software,
                    _ => {
                        return Err(format!(
                            "Unknown renderer {name:?}; use femtovg-wgpu or software"
                        ));
                    }
                }
            } else {
                continue;
            };
            if selected.is_some_and(|old| old != renderer) {
                return Err("Conflicting renderer flags".into());
            }
            selected = Some(renderer);
        }
        Ok(selected.unwrap_or_default())
    }
    pub fn select(self) -> Result<(), slint::PlatformError> {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name(self.name().into())
            .select()
            .map_err(|error| {
                slint::PlatformError::from(format!(
                    "Unable to initialize {}: {error}. Try --software.",
                    self.name()
                ))
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_renderer_alias_and_conflicts() {
        assert_eq!(Renderer::from_args(["app"]).unwrap(), Renderer::FemtoVgWgpu);
        assert_eq!(
            Renderer::from_args(["--software", "--renderer=software"]).unwrap(),
            Renderer::Software
        );
        assert!(Renderer::from_args(["--renderer=femtovg-wgpu", "--software"]).is_err());
        assert!(Renderer::from_args(["--renderer=skia"]).is_err());
    }
}
