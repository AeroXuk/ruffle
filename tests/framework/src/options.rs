use crate::backends::TestAudioBackend;
use crate::environment::{Environment, RenderInterface};
use crate::image_trigger::ImageTrigger;
use crate::util::write_image;
use anyhow::{anyhow, Result};
use approx::relative_eq;
use image::ImageFormat;
use regex::Regex;
use ruffle_core::tag_utils::SwfMovie;
use ruffle_core::{PlayerBuilder, PlayerMode, PlayerRuntime, ViewportDimensions};
use ruffle_render::backend::RenderBackend;
use ruffle_render::quality::StageQuality;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use vfs::VfsPath;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestOptions {
    pub num_frames: Option<u32>,
    pub num_ticks: Option<u32>,
    pub tick_rate: Option<f64>,
    pub output_path: String,
    pub sleep_to_meet_frame_rate: bool,
    pub image_comparisons: HashMap<String, ImageComparison>,
    pub ignore: bool,
    pub known_failure: bool,
    pub approximations: Option<Approximations>,
    pub player_options: PlayerOptions,
    pub log_fetch: bool,
    pub required_features: RequiredFeatures,
    pub fonts: HashMap<String, FontOptions>,
}

impl Default for TestOptions {
    fn default() -> Self {
        Self {
            num_frames: None,
            num_ticks: None,
            tick_rate: None,
            output_path: "output.txt".to_string(),
            sleep_to_meet_frame_rate: false,
            image_comparisons: Default::default(),
            ignore: false,
            known_failure: false,
            approximations: None,
            player_options: PlayerOptions::default(),
            log_fetch: false,
            required_features: RequiredFeatures::default(),
            fonts: Default::default(),
        }
    }
}

impl TestOptions {
    pub fn read(path: &VfsPath) -> Result<Self> {
        let result: Self = toml::from_str(&path.read_to_string()?)?;
        result.validate()?;
        Ok(result)
    }

    fn validate(&self) -> Result<()> {
        if !self.image_comparisons.is_empty() {
            let mut seen_triggers = HashSet::new();
            for comparison in self.image_comparisons.values() {
                if comparison.trigger != ImageTrigger::FsCommand
                    && !seen_triggers.insert(comparison.trigger)
                {
                    return Err(anyhow!(
                        "Multiple captures are set to trigger {:?}. This likely isn't intended!",
                        comparison.trigger
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn output_path(&self, test_directory: &VfsPath) -> Result<VfsPath> {
        Ok(test_directory.join(&self.output_path)?)
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Approximations {
    number_patterns: Vec<String>,
    epsilon: Option<f64>,
    max_relative: Option<f64>,
}

impl Approximations {
    pub fn compare(&self, actual: f64, expected: f64) -> Result<()> {
        let result = match (self.epsilon, self.max_relative) {
            (Some(epsilon), Some(max_relative)) => relative_eq!(
                actual,
                expected,
                epsilon = epsilon,
                max_relative = max_relative
            ),
            (Some(epsilon), None) => relative_eq!(actual, expected, epsilon = epsilon),
            (None, Some(max_relative)) => {
                relative_eq!(actual, expected, max_relative = max_relative)
            }
            (None, None) => relative_eq!(actual, expected),
        };

        if result {
            Ok(())
        } else {
            Err(anyhow!(
                "Approximation failed: expected {}, found {}. Episilon = {:?}, Max Relative = {:?}",
                expected,
                actual,
                self.epsilon,
                self.max_relative
            ))
        }
    }

    pub fn number_patterns(&self) -> Vec<Regex> {
        self.number_patterns
            .iter()
            .map(|p| Regex::new(p).unwrap())
            .collect()
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RequiredFeatures {
    lzma: bool,
    jpegxr: bool,
}

impl RequiredFeatures {
    pub fn can_run(&self) -> bool {
        (!self.lzma || cfg!(feature = "lzma")) && (!self.jpegxr || cfg!(feature = "jpegxr"))
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PlayerOptions {
    max_execution_duration: Option<Duration>,
    viewport_dimensions: Option<ViewportDimensions>,
    with_renderer: Option<RenderOptions>,
    with_audio: bool,
    with_video: bool,
    runtime: PlayerRuntime,
    mode: Option<PlayerMode>,
}

impl PlayerOptions {
    pub fn setup(&self, mut player_builder: PlayerBuilder) -> Result<PlayerBuilder> {
        if let Some(max_execution_duration) = self.max_execution_duration {
            player_builder = player_builder.with_max_execution_duration(max_execution_duration);
        }

        if let Some(render_options) = &self.with_renderer {
            player_builder = player_builder.with_quality(match render_options.sample_count {
                16 => StageQuality::High16x16,
                8 => StageQuality::High8x8,
                4 => StageQuality::High,
                2 => StageQuality::Medium,
                _ => StageQuality::Low,
            });
        }

        if self.with_audio {
            player_builder = player_builder.with_audio(TestAudioBackend::default());
        }

        player_builder = player_builder
            .with_player_runtime(self.runtime)
            // Assume flashplayerdebugger is used in tests
            .with_player_mode(self.mode.unwrap_or(PlayerMode::Debug));

        if self.with_video {
            #[cfg(feature = "ruffle_video_external")]
            {
                let current_exe = std::env::current_exe()?;
                let directory = current_exe.parent().expect("Executable parent dir");

                use ruffle_video_external::{
                    backend::ExternalVideoBackend, decoder::openh264::OpenH264Codec,
                };
                let openh264 = OpenH264Codec::load(directory)
                    .map_err(|e| anyhow!("Couldn't load OpenH264: {}", e))?;

                player_builder =
                    player_builder.with_video(ExternalVideoBackend::new_with_openh264(openh264));
            }

            #[cfg(all(
                not(feature = "ruffle_video_external"),
                feature = "ruffle_video_software"
            ))]
            {
                player_builder = player_builder
                    .with_video(ruffle_video_software::backend::SoftwareVideoBackend::new());
            }
        }

        Ok(player_builder)
    }

    pub fn can_run(&self, check_renderer: bool, environment: &impl Environment) -> bool {
        if let Some(render) = &self.with_renderer {
            // If we don't actually want to check the renderer (ie we're just listing potential tests),
            // don't spend the cost to create it
            if check_renderer && !render.optional && !environment.is_render_supported(render) {
                return false;
            }
        }
        true
    }

    pub fn viewport_dimensions(&self, movie: &SwfMovie) -> ViewportDimensions {
        self.viewport_dimensions
            .unwrap_or_else(|| ViewportDimensions {
                width: movie.width().to_pixels() as u32,
                height: movie.height().to_pixels() as u32,
                scale_factor: 1.0,
            })
    }

    pub fn create_renderer(
        &self,
        environment: &impl Environment,
        dimensions: ViewportDimensions,
    ) -> Option<(Box<dyn RenderInterface>, Box<dyn RenderBackend>)> {
        if self.with_renderer.is_some() {
            environment.create_renderer(dimensions.width, dimensions.height)
        } else {
            None
        }
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ImageComparison {
    tolerance: Option<u8>,
    max_outliers: Option<usize>,
    checks: Vec<ImageComparisonCheck>,
    pub trigger: ImageTrigger,
}

fn calc_difference(lhs: u8, rhs: u8) -> u8 {
    (lhs as i16 - rhs as i16).unsigned_abs() as u8
}

impl ImageComparison {
    fn checks(&self) -> Result<Cow<'_, [ImageComparisonCheck]>> {
        let has_simple_check = self.tolerance.is_some() || self.max_outliers.is_some();
        if has_simple_check && !self.checks.is_empty() {
            return Err(anyhow!(
                "Both simple and advanced checks are defined. \
                Either remove 'tolerance' & 'max_outliers', or move it to 'checks'."
            ));
        }

        if !self.checks.is_empty() {
            Ok(Cow::Borrowed(&self.checks))
        } else {
            Ok(Cow::Owned(vec![ImageComparisonCheck {
                tolerance: self.tolerance.unwrap_or_default(),
                max_outliers: self.max_outliers.unwrap_or_default(),
                filter: None,
            }]))
        }
    }

    pub fn test(
        &self,
        name: &str,
        actual_image: image::RgbaImage,
        expected_image: image::RgbaImage,
        test_path: &VfsPath,
        environment_name: String,
        known_failure: bool,
    ) -> Result<()> {
        use anyhow::Context;

        let save_actual_image = || {
            if !known_failure {
                // If we're expecting failure, spamming files isn't productive.
                write_image(
                    &test_path.join(format!("{name}.actual-{environment_name}.png"))?,
                    &actual_image,
                    ImageFormat::Png,
                )
            } else {
                Ok(())
            }
        };

        if actual_image.width() != expected_image.width()
            || actual_image.height() != expected_image.height()
        {
            save_actual_image()?;
            return Err(anyhow!(
                "'{}' image is not the right size. Expected = {}x{}, actual = {}x{}.",
                name,
                expected_image.width(),
                expected_image.height(),
                actual_image.width(),
                actual_image.height()
            ));
        }

        let mut is_alpha_different = false;

        let difference_data: Vec<u8> = Self::calculate_difference_data(
            &actual_image,
            &expected_image,
            &mut is_alpha_different,
        );

        let checks = self
            .checks()
            .map_err(|err| anyhow!("Image '{name}' failed: {err}"))?;

        let mut any_check_executed = false;
        for (i, check) in checks.iter().enumerate() {
            let check_name = format!("Image '{name}' check {i}");
            let filter_passed = check
                .filter
                .as_ref()
                .map(|f| f.evaluate())
                .unwrap_or(Ok(true))?;
            if !filter_passed {
                println!("{check_name} skipped: Filtered out.");
                continue;
            }

            let outliers = Self::calculate_outliers(&difference_data, check.tolerance);
            let max_outliers = check.max_outliers;
            let max_difference = Self::calculate_max_difference(&difference_data);

            any_check_executed = true;
            if outliers <= max_outliers {
                println!("{check_name} succeeded: {outliers} outliers found, max difference {max_difference}");
                continue;
            }

            // The image failed a check :(

            save_actual_image()?;

            let mut difference_color = Vec::with_capacity(
                actual_image.width() as usize * actual_image.height() as usize * 3,
            );
            for p in difference_data.chunks_exact(4) {
                difference_color.extend_from_slice(&p[..3]);
            }

            if !known_failure {
                // If we're expecting failure, spamming files isn't productive.
                let difference_image = image::RgbImage::from_raw(
                    actual_image.width(),
                    actual_image.height(),
                    difference_color,
                )
                .context("Couldn't create color difference image")?;
                write_image(
                    &test_path.join(format!("{name}.difference-color-{environment_name}.png"))?,
                    &difference_image,
                    ImageFormat::Png,
                )?;
            }

            if is_alpha_different {
                let mut difference_alpha = Vec::with_capacity(
                    actual_image.width() as usize * actual_image.height() as usize,
                );
                for p in difference_data.chunks_exact(4) {
                    difference_alpha.push(p[3])
                }

                if !known_failure {
                    // If we're expecting failure, spamming files isn't productive.
                    let difference_image = image::GrayImage::from_raw(
                        actual_image.width(),
                        actual_image.height(),
                        difference_alpha,
                    )
                    .context("Couldn't create alpha difference image")?;
                    write_image(
                        &test_path
                            .join(format!("{name}.difference-alpha-{environment_name}.png"))?,
                        &difference_image,
                        ImageFormat::Png,
                    )?;
                }
            }

            return Err(anyhow!(
                "{check_name} failed: \
                Number of outliers ({outliers}) is bigger than allowed limit of {max_outliers}. \
                Max difference is {max_difference}",
            ));
        }

        if !any_check_executed {
            return Err(anyhow!("Image '{name}' failed: No checks executed.",));
        }

        Ok(())
    }

    fn calculate_difference_data(
        actual_image: &image::RgbaImage,
        expected_image: &image::RgbaImage,
        is_alpha_different: &mut bool,
    ) -> Vec<u8> {
        expected_image
            .as_raw()
            .chunks_exact(4)
            .zip(actual_image.as_raw().chunks_exact(4))
            .flat_map(|(cmp_chunk, data_chunk)| {
                if cmp_chunk[3] != data_chunk[3] {
                    *is_alpha_different = true;
                }

                [
                    calc_difference(cmp_chunk[0], data_chunk[0]),
                    calc_difference(cmp_chunk[1], data_chunk[1]),
                    calc_difference(cmp_chunk[2], data_chunk[2]),
                    calc_difference(cmp_chunk[3], data_chunk[3]),
                ]
            })
            .collect()
    }

    fn calculate_outliers(difference_data: &[u8], tolerance: u8) -> usize {
        difference_data
            .chunks_exact(4)
            .map(|colors| {
                (colors[0] > tolerance) as usize
                    + (colors[1] > tolerance) as usize
                    + (colors[2] > tolerance) as usize
                    + (colors[3] > tolerance) as usize
            })
            .sum()
    }

    fn calculate_max_difference(difference_data: &[u8]) -> u8 {
        difference_data
            .chunks_exact(4)
            .map(|colors| colors[0].max(colors[1]).max(colors[2]).max(colors[3]))
            .max()
            .unwrap()
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
struct ImageComparisonCheck {
    tolerance: u8,
    max_outliers: usize,

    filter: Option<TestExpression>,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderOptions {
    optional: bool,
    pub sample_count: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            optional: false,
            sample_count: 1,
        }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct FontOptions {
    pub family: String,
    pub path: String,
    pub bold: bool,
    pub italic: bool,
}

/// Test expression is a cfg-like expression that evaluates to a boolean
/// and can be used in test configuration.
///
/// Currently the following variables are supported:
/// * `os` --- refers to [`std::env::consts::OS`],
/// * `arch` --- refers to [`std::env::consts::ARCH`],
/// * `family` --- refers to [`std::env::consts::FAMILY`].
///
/// Example expression:
///
/// ```text
/// not(os = "aarch64")
/// ```
#[derive(Deserialize, Clone, Debug)]
struct TestExpression(String);

impl TestExpression {
    fn evaluate(&self) -> Result<bool> {
        let cfg_parsed = cfg_expr::Expression::parse(&self.0)
            .map_err(|err| anyhow!("Cannot parse expression:\n{err}"))?;
        let mut unknown_pred = None;
        let cfg_matches = cfg_parsed.eval(|pred| match pred {
            cfg_expr::Predicate::KeyValue { key, val } if *key == "os" => {
                *val == std::env::consts::OS
            }
            cfg_expr::Predicate::KeyValue { key, val } if *key == "arch" => {
                *val == std::env::consts::ARCH
            }
            cfg_expr::Predicate::KeyValue { key, val } if *key == "family" => {
                *val == std::env::consts::FAMILY
            }
            _ => {
                unknown_pred = Some(format!("{pred:?}"));
                false
            }
        });
        if let Some(pred) = unknown_pred {
            return Err(anyhow!("Unknown predicate used in expression: {pred}"));
        }
        Ok(cfg_matches)
    }
}
