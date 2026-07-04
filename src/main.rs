use bevy::{
    camera::Exposure,
    camera_controller::free_camera::{FreeCamera, FreeCameraPlugin},
    color::palettes::css::{GREEN, RED},
    core_pipeline::tonemapping::Tonemapping,
    dev_tools::diagnostics_overlay::{DiagnosticsOverlay, DiagnosticsOverlayPlugin},
    diagnostic::FrameTimeDiagnosticsPlugin,
    image::{
        ImageAddressMode, ImageFilterMode, ImageLoaderSettings, ImageSampler,
        ImageSamplerDescriptor,
    },
    light::{
        Atmosphere, AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder, SunDisk,
        atmosphere::ScatteringMedium, light_consts::lux,
    },
    math::{DVec3, ops::atan2},
    pbr::{
        AtmosphereSettings, ExtendedMaterial, MaterialExtension,
        wireframe::{WireframeConfig, WireframePlugin},
    },
    post_process::bloom::Bloom,
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
    tasks::{AsyncComputeTaskPool, Task, futures_lite::future},
};
use big_space::commands::BigSpaceCommands;
use big_space::prelude::*;
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{f32::consts::PI, fs::File, io::Write, path::Path, sync::Arc};
use tokio::{runtime::Runtime, sync::Semaphore};

fn main() {
    let default_plugins = DefaultPlugins.build().disable::<TransformPlugin>();
    App::new()
        .insert_resource(ClearColor(Color::LinearRgba(LinearRgba {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        })))
        .insert_resource(GlobalAmbientLight::NONE)
        .add_plugins((
            default_plugins,
            BigSpaceDefaultPlugins,
            MaterialPlugin::<ExtendedMaterial<StandardMaterial, TerrainMaterial>>::default(),
            FreeCameraPlugin,
            FrameTimeDiagnosticsPlugin::default(),
            DiagnosticsOverlayPlugin,
            WireframePlugin::default(),
        ))
        .add_systems(Startup, setup)
        .add_systems(Update, (update, poll_terrain))
        // .insert_resource(WireframeConfig {
        //     global: true,
        //     default_color: RED.into(),
        //     ..default()
        // })
        .run();
}

const EARTH_RADIUS: f32 = 6_360_000.0;

const SIZE: f32 = 2.0;
const SUBDIV: u16 = 8;
const CHUNKS: u16 = SUBDIV.pow(2);

#[derive(Component)]
struct Camera;

fn setup(mut commands: Commands, mut scattering_mediums: ResMut<Assets<ScatteringMedium>>) {
    let cascade = CascadeShadowConfigBuilder {
        maximum_distance: 5000.0,
        ..Default::default()
    }
    .build();

    let earth_medium = scattering_mediums.add(ScatteringMedium::earth(256, 256));

    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            illuminance: lux::RAW_SUNLIGHT,
            ..default()
        },
        cascade,
        SunDisk::EARTH,
        Transform::from_xyz(1.0, 1.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    commands.spawn(DiagnosticsOverlay::fps());

    commands.spawn_big_space_default(|mut parent| {
        let (cell_coord, cell_offset) =
            parent
                .grid()
                .translation_to_grid(DVec3::new(0.0, EARTH_RADIUS as f64, 0.0));

        parent.spawn_spatial((
            bevy::ui::prelude::IsDefaultUiCamera,
            cell_coord,
            Camera3d::default(),
            Tonemapping::AcesFitted,
            Bloom::NATURAL,
            AtmosphereEnvironmentMapLight::default(),
            AtmosphereSettings {
                rendering_method: bevy::pbr::AtmosphereMode::Raymarched,
                ..default()
            },
            Camera,
            Exposure { ev100: 13.0 },
            bevy::camera::Hdr,
            FreeCamera {
                walk_speed: 10000.0,
                run_speed: 1000000.0,
                ..default()
            },
            FloatingOrigin,
            Transform::from_translation(cell_offset + Vec3::new(0.0, 0.0, 10.0)),
        ));

        parent.spawn_spatial((
            Atmosphere::earth(earth_medium),
            CellCoord::default(),
            Transform::from_xyz(0.0, 0.0, 0.0),
        ));
        let normals = vec![
            Dir3::X,
            Dir3::Y,
            Dir3::Z,
            Dir3::NEG_X,
            Dir3::NEG_Y,
            Dir3::NEG_Z,
        ];

        let client = Client::new();
        let semaphore = Arc::new(Semaphore::new(12));
        spawn_chunk(&mut parent, normals[1], &client, Arc::clone(&semaphore));
    });
}

const SHADER_ASSET_PATH: &str = "shaders/terrain.wgsl";

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub struct TerrainMaterial {
    #[texture(101)]
    #[sampler(102)]
    pub normals: Handle<Image>,
}

impl MaterialExtension for TerrainMaterial {
    fn deferred_fragment_shader() -> ShaderRef {
        SHADER_ASSET_PATH.into()
    }
}

#[derive(Component)]
struct TerrainFaces;

#[derive(Component)]
pub struct SpawnTerrain(Task<Option<Mesh>>, (CellCoord, Vec3));

struct Coords {
    lat: f32,
    long: f32,
}

fn coords_to_terrarium_coords(
    coords: Coords,
    zoom: u8,
) -> Result<TerrariumCoords, Box<dyn std::error::Error>> {
    if coords.lat < -85.05113 || coords.lat > 85.05113 {
        return Err(format!(
            "Latitude {} is out of Web Mercator bounds (-85.05113..85.05113)",
            coords.lat
        )
        .into());
    }
    if coords.long < -180.0 || coords.long > 180.0 {
        return Err(format!("Longitude {} is out of bounds (-180.0..180.0)", coords.long).into());
    }

    let z = zoom as f32;
    let n = 2.0_f32.powf(z);

    let x = n * ((coords.long + 180.0) / 360.0);
    let lat_rad = (coords.lat).to_radians();
    let y = (1.0 - (lat_rad.tan() + (1.0 / lat_rad.cos())).ln() / PI) / 2.0 * n;

    // rounding down
    let tile_x = x.floor() as u32;
    let tile_y = y.floor() as u32;

    Ok(TerrariumCoords {
        z: zoom,
        x: tile_x,
        y: tile_y,
    })
}

pub fn spawn_chunk(
    commands: &mut GridCommands,
    normal: Dir3,
    client: &Client,
    semaphore: Arc<Semaphore>,
) {
    let thread_pool = AsyncComputeTaskPool::get();

    let chunk_size = SIZE / SUBDIV as f32;
    let centre_offset = (SUBDIV as f32 - 1.0) * 0.5;

    // left to right,
    // top to bottom
    for i in 0..CHUNKS {
        let ix = (i % SUBDIV) as f32;
        let iy = (i / SUBDIV) as f32;

        let a = (ix - centre_offset) * chunk_size;
        let b = (iy - centre_offset) * chunk_size;

        let mut translation_per_chunk = Vec3::ZERO;
        if normal == Dir3::NEG_X || normal == Dir3::X {
            translation_per_chunk.y = a;
            translation_per_chunk.z = b;
        }
        if normal == Dir3::NEG_Y || normal == Dir3::Y {
            translation_per_chunk.x = a;
            translation_per_chunk.z = b;
        }
        if normal == Dir3::NEG_Z || normal == Dir3::Z {
            translation_per_chunk.x = a;
            translation_per_chunk.y = b;
        }

        let chunk_translation = Vec3 {
            x: normal.x,
            y: normal.y,
            z: normal.z,
        } + translation_per_chunk;

        let client_clone = client.clone();
        let semaphore_clone = Arc::clone(&semaphore);

        let tokio_handle = TOKIO_RUNTIME.spawn(build_mesh(
            normal,
            chunk_translation,
            client_clone,
            semaphore_clone,
        ));

        let task = thread_pool.spawn(async move { tokio_handle.await.unwrap() });
        let (cell_coord, cell_offset) = commands.grid().translation_to_grid(chunk_translation);
        commands.spawn(SpawnTerrain(task, (cell_coord, cell_offset)));
    }
}

fn to_sphere_pos(pos: &[f32; 3]) -> Vec3 {
    let p = Vec3 {
        x: pos[0],
        y: pos[1],
        z: pos[2],
    };

    // Even spacing of vertices on sphere
    let x =
        p.x * (1.0 - (p.y.powi(2) + p.z.powi(2)) / 2.0 + (p.y.powi(2) * p.z.powi(2) / 3.0)).sqrt();
    let y =
        p.y * (1.0 - (p.z.powi(2) + p.x.powi(2)) / 2.0 + (p.z.powi(2) * p.x.powi(2) / 3.0)).sqrt();
    let z =
        p.z * (1.0 - (p.x.powi(2) + p.y.powi(2)) / 2.0 + (p.x.powi(2) * p.y.powi(2) / 3.0)).sqrt();
    let even_spaced_pos = Vec3::new(x, y, z);

    even_spaced_pos
}

static TOKIO_RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Failed to create tokio runtime"));
fn update() {}

async fn build_mesh(
    normal: Dir3,
    chunk_translation: Vec3,
    client: Client,
    semaphore: Arc<Semaphore>,
) -> Option<Mesh> {
    let mut earth_mesh = Mesh::from(
        Plane3d::default()
            .mesh()
            .size(SIZE / SUBDIV as f32, SIZE / SUBDIV as f32)
            .normal(normal)
            .subdivisions(512 - 2),
    )
    .translated_by(chunk_translation);
    let coord = if let bevy::mesh::VertexAttributeValues::Float32x3(positions) = earth_mesh
        .try_attribute_mut(Mesh::ATTRIBUTE_POSITION)
        .unwrap()
    {
        let pos = positions[0];
        let distance_h = (pos[0].powi(2) + pos[2].powi(2)).sqrt();

        let bearing = pos[0].atan2(pos[2]).to_degrees();

        let elevation = pos[1]
            .atan2(distance_h)
            .to_degrees()
            .clamp(-85.05113, 85.05113);

        let coords = Coords {
            lat: elevation,
            long: bearing,
        };
        coords_to_terrarium_coords(coords, 5).unwrap()
    } else {
        return None;
    };
    match get_elevation(&client, semaphore, &coord).await {
        Ok(_) => {}
        Err(e) => {
            error!("get_elevation: {}", e);
            return None;
        }
    }

    let path = format!("terrain_cache/{}_{}_{}.webp", coord.z, coord.x, coord.y);
    let img = match image::load_from_memory_with_format(
        &std::fs::read(&path).expect("file to be fetched"),
        image::ImageFormat::WebP,
    ) {
        Ok(img) => img,
        Err(e) => {
            error!("Failed to load WebP image from path {}: {}", path, e);
            return None;
        }
    };

    let rgb_img = img.to_rgb8();
    let (width, height) = rgb_img.dimensions();
    let mut heights: std::vec::Vec<f32> = Vec::with_capacity((width * height) as usize);

    for pixel in rgb_img.pixels() {
        let r = pixel[0] as f32;
        let g = pixel[1] as f32;
        let b = pixel[2] as f32;

        let h = (r * 256.0 + g + b / 256.0) - 32768.0;
        heights.push(h);
    }

    if !heights.windows(2).all(|w| w[0] == w[1]) {
        // make the planes a sphere
        if let bevy::mesh::VertexAttributeValues::Float32x3(positions) = earth_mesh
            .try_attribute_mut(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
        {
            assert_eq!(positions.len(), heights.len());

            for (i, pos) in positions.iter_mut().enumerate() {
                let even_spaced_pos = to_sphere_pos(&pos);
                *pos = (even_spaced_pos).to_array();

                let factor = 1.0 + (0.0000002 * heights[i]);
                pos[0] *= factor;
                pos[1] *= factor;
                pos[2] *= factor;
            }
        } else {
            return None;
        }

        earth_mesh.compute_normals();

        return Some(earth_mesh);
    }

    None
}

async fn get_elevation(
    client: &Client,
    semaphore: Arc<Semaphore>,
    coord: &TerrariumCoords,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_name = format!("terrain_cache/{}_{}_{}.webp", coord.z, coord.x, coord.y);

    if !Path::new(&file_name).exists() {
        let _permit = semaphore.acquire().await?;

        let url = format!(
            "https://tiles.mapterhorn.com/{}/{}/{}.webp",
            coord.z, coord.x, coord.y
        );

        let response = client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(format!(
                "Server responded to {} with status: {}",
                &url,
                response.status()
            )
            .into());
        }

        let bytes = response.bytes().await?;

        let mut file = File::create(file_name)?;
        file.write_all(&bytes)?;
    }

    Ok(())
}

#[derive(Clone, Debug)]
pub struct TerrariumCoords {
    z: u8,
    x: u32,
    y: u32,
}

fn poll_terrain(
    mut tasks: Query<(Entity, &mut SpawnTerrain)>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, TerrainMaterial>>>,
    asset_server: Res<AssetServer>,
    big_space: Single<Entity, With<BigSpace>>,
) {
    for (entity, mut task) in &mut tasks {
        if let Some(mesh_intermediary) = future::block_on(future::poll_once(&mut task.0)) {
            if let Some(earth_mesh) = mesh_intermediary {
                let (cell_coord, cell_offset) = task.1;

                let chunk = commands
                    .spawn((
                        TerrainFaces,
                        Mesh3d(meshes.add(earth_mesh)),
                        MeshMaterial3d(
                            terrain_materials.add(ExtendedMaterial {
                                base: StandardMaterial {
                                    base_color: Color::Srgba(GREEN),
                                    perceptual_roughness: 1.0,
                                    ..Default::default()
                                },
                                extension: TerrainMaterial {
                                    normals: asset_server
                                        .load_builder()
                                        .with_settings(|settings: &mut ImageLoaderSettings| {
                                            settings.is_srgb = false;
                                            settings.sampler =
                                                ImageSampler::Descriptor(ImageSamplerDescriptor {
                                                    address_mode_u: ImageAddressMode::Repeat,
                                                    address_mode_v: ImageAddressMode::Repeat,
                                                    mag_filter: ImageFilterMode::Linear,
                                                    min_filter: ImageFilterMode::Linear,
                                                    ..default()
                                                });
                                        })
                                        .load("textures/water_normals.png"),
                                },
                            }),
                        ),
                        Transform::from_translation(
                            cell_offset
                                + Vec3 {
                                    x: 0.0,
                                    y: 0.0,
                                    z: 0.0,
                                },
                        )
                        .with_scale(Vec3 {
                            x: EARTH_RADIUS,
                            y: EARTH_RADIUS,
                            z: EARTH_RADIUS,
                        }),
                        cell_coord,
                    ))
                    .id();

                commands.entity(*big_space).add_child(chunk);
            }
            commands.entity(entity).remove::<SpawnTerrain>();
        }
    }
}
