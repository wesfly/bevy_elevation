use bevy::{
    anti_alias::taa::TemporalAntiAliasing,
    camera::Exposure,
    camera_controller::free_camera::{FreeCamera, FreeCameraPlugin},
    color::palettes::css::GREEN,
    core_pipeline::{
        prepass::{DeferredPrepass, DepthPrepass},
        tonemapping::Tonemapping,
    },
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
    math::DVec3,
    pbr::{AtmosphereSettings, ExtendedMaterial, MaterialExtension, wireframe::WireframePlugin},
    post_process::bloom::Bloom,
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
    tasks::{AsyncComputeTaskPool, Task, futures_lite::future},
};
use big_space::commands::BigSpaceCommands;
use big_space::prelude::*;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{collections::HashSet, f32::consts::PI, fs::File, io::Write, path::Path, sync::Arc};
use tokio::{runtime::Runtime, sync::Semaphore};

#[allow(unused)] // For wireframe
use bevy::{color::palettes::css::RED, pbr::wireframe::WireframeConfig};

fn main() {
    let default_plugins = DefaultPlugins.build().disable::<TransformPlugin>();
    App::new()
        .insert_resource(ClearColor(Color::LinearRgba(LinearRgba {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        })))
        .insert_resource(TerrainCacheResource::default())
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
        .add_systems(Update, (update, poll_terrain, rotate_sun))
        // .insert_resource(WireframeConfig {
        //     global: true,
        //     default_color: RED.into(),
        //     ..default()
        // })
        .run();
}

const EARTH_RADIUS: f32 = 6_360_000.0;

const SIZE: f32 = 2.0;
const SUBDIV: u32 = 4096 * 2;
const CHUNKS: u32 = SUBDIV.pow(2);
const ZOOM: u8 = 14;
const SUBDIV_PER_TILE: u32 = 64;
const VIEW_RADIUS: f32 = 20_000.0;

#[derive(Component)]
struct Camera;

#[derive(Resource, Clone)]
pub struct TerrainCacheResource {
    pub cache: TileCache,
}
impl Default for TerrainCacheResource {
    fn default() -> Self {
        return Self {
            cache: Arc::new(DashMap::new()),
        };
    }
}

fn setup(
    mut commands: Commands,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
    cache: Res<TerrainCacheResource>,
) {
    init_cache();
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
            (
                Msaa::Off,
                DepthPrepass,
                DeferredPrepass,
                TemporalAntiAliasing::default(),
            ),
            // ScreenSpaceReflections {
            //     min_perceptual_roughness: 0.0..0.0,
            //     ..default()
            // },
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
        let semaphore = Arc::new(Semaphore::new(64));
        for normal in normals {
            spawn_chunk(
                &mut parent,
                normal,
                // normals[1],
                &client,
                Arc::clone(&semaphore),
                cache.cache.clone(),
            );
        }
    });
}

const SHADER_ASSET_PATH: &str = "shaders/terrain.wgsl";

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub struct TerrainMaterial {
    #[texture(100)]
    #[sampler(101)]
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

struct Coord {
    lat: f32,
    long: f32,
}

pub fn spawn_chunk(
    commands: &mut GridCommands,
    normal: Dir3,
    client: &Client,
    semaphore: Arc<Semaphore>,
    cache: TileCache,
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

        let target_coord = Coord {
            lat: 42.53436,
            long: 8.79284,
        };

        let lat_rad = target_coord.lat.to_radians();
        let long_rad = target_coord.long.to_radians();

        let y = lat_rad.sin();
        let x = lat_rad.cos() * long_rad.sin();
        let z = lat_rad.cos() * long_rad.cos();

        let projected_chunk_center = to_sphere_pos(&chunk_translation.to_array());

        if projected_chunk_center
            .normalize()
            .distance(Vec3 { x, y, z })
            > VIEW_RADIUS / EARTH_RADIUS
        {
            continue;
        }

        let client_clone = client.clone();
        let semaphore_clone = Arc::clone(&semaphore);

        let tokio_handle = TOKIO_RUNTIME.spawn(build_mesh(
            normal,
            chunk_translation,
            client_clone,
            semaphore_clone,
            Arc::clone(&cache),
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

    let x2 = p.x * p.x;
    let y2 = p.y * p.y;
    let z2 = p.z * p.z;

    // Even spacing of vertices on sphere
    let x = p.x * (1.0 - (y2 + z2) / 2.0 + (y2 * z2 / 3.0)).sqrt();
    let y = p.y * (1.0 - (z2 + x2) / 2.0 + (z2 * x2 / 3.0)).sqrt();
    let z = p.z * (1.0 - (x2 + y2) / 2.0 + (x2 * y2 / 3.0)).sqrt();
    let even_spaced_pos = Vec3::new(x, y, z);

    even_spaced_pos * EARTH_RADIUS
}

// type TileCache = Arc<RwLock<HashMap<(u8, u32, u32), Arc<RgbImage>>>>;
type TileCache = Arc<DashMap<(u8, u32, u32), Arc<image::RgbImage>>>;
pub fn init_cache() -> TileCache {
    Arc::new(DashMap::new())
}

static DUMMY_TILE: Lazy<Arc<image::RgbImage>> = Lazy::new(|| {
    let mut dummy = image::RgbImage::new(512, 512);
    for pixel in dummy.pixels_mut() {
        *pixel = image::Rgb([128, 0, 0]);
    }
    Arc::new(dummy)
});

fn coord_to_tile(coord: Coord, n: f32) -> (u32, u32) {
    // Longitude to Tile X
    let x = n * ((coord.long + 180.0) / 360.0);

    // Latitude to Tile Y (clamped to protect against tangents approaching infinity near poles)
    let lat_rad = coord
        .lat
        .to_radians()
        .clamp(-85.05112_f32.to_radians(), 85.05112_f32.to_radians());
    let y = (1.0 - (lat_rad.tan() + (1.0 / lat_rad.cos())).ln() / std::f32::consts::PI) / 2.0 * n;

    (x.floor() as u32, y.floor() as u32)
}

async fn ensure_tiles_loaded(
    client: &Client,
    semaphore: Arc<tokio::sync::Semaphore>,
    cache: TileCache,
    required_tiles: Vec<(u8, u32, u32)>,
) {
    let mut fetch_tasks = vec![];

    for (zoom, x, y) in required_tiles {
        if cache.contains_key(&(zoom, x, y)) {
            continue;
        }

        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);
        let cache = Arc::clone(&cache);

        let task = tokio::spawn(async move {
            let path = format!("terrain_cache/{}_{}_{}.webp", zoom, x, y);

            match get_tile(&client, semaphore, &TerrariumCoords { z: zoom, x, y }).await {
                Ok(_) => {}
                Err(e) => {
                    warn!("Missing tile {}/{}/{}: {}", zoom, x, y, e);
                }
            }

            let img_result = tokio::task::spawn_blocking(move || {
                let bytes = std::fs::read(&path).unwrap_or_else(|_| vec![]);

                if bytes.is_empty() {
                    return Ok::<Arc<image::RgbImage>, String>(Arc::clone(&DUMMY_TILE));
                }

                match image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP) {
                    Ok(img) => Ok(Arc::new(img.to_rgb8())),
                    Err(_) => Ok(Arc::clone(&DUMMY_TILE)),
                }
            })
            .await
            .expect("Task panicked");

            let final_image = img_result.unwrap_or_else(|_| Arc::clone(&DUMMY_TILE));
            cache.insert((zoom, x, y), final_image);
        });

        fetch_tasks.push(task);
    }

    futures::future::join_all(fetch_tasks).await;
}

fn get_height_at_coord(coord_: Coord, zoom: u8, cache: &TileCache) -> f32 {
    //--------------------- coords to terrarium coords ---------------------

    // because coordinates are from elliptic sphere (geodetic coords)
    // const FLATTENING_SQ: f32 = 0.99330562;

    // let geocentric_lat_rad = coord.lat.to_radians();

    // // Convert geocentric latitude to geodetic latitude
    // let geodetic_lat_rad = (geocentric_lat_rad.tan() / FLATTENING_SQ).atan();
    // let geodetic_lat = geodetic_lat_rad.to_degrees();

    // if geodetic_lat < -85.05113 || coord.lat > 85.05113 {
    //     error!(
    //         "Latitude {} is out of Web Mercator bounds (-85.05113..85.05113)",
    //         coord.lat
    //     );
    //     return 0.0;
    // }
    let coord = coord_;
    if coord.long < -180.0 || coord.long > 180.0 {
        error!("Longitude {} is out of bounds (-180.0..180.0)", coord.long);
        return 0.0;
    }

    let z = zoom as f32;
    let n = 2.0_f32.powf(z);

    let x = n * ((coord.long + 180.0) / 360.0);
    let y = (1.0 - (coord.lat.to_radians().tan() + (1.0 / coord.lat.to_radians().cos())).ln() / PI)
        / 2.0
        * n;

    // rounding down
    let tile_x = x.floor() as u32;
    let tile_y = y.floor() as u32;

    // we do all of this instead of calling the function for these two values
    let offset_x = x - tile_x as f32;
    let offset_y = y - tile_y as f32;

    //--------------------- sample elevation  ---------------------

    let px_offset_x = (offset_x * 512.0) as u32;
    let px_offset_y = (offset_y * 512.0) as u32;

    if let Some(img) = cache.get(&(zoom, tile_x, tile_y)) {
        // Double check bounds just in case of float precision weirdness
        if px_offset_x < 512 && px_offset_y < 512 {
            let pixel = img[(px_offset_x, px_offset_y)];
            let r = pixel[0] as f32;
            let g = pixel[1] as f32;
            let b = pixel[2] as f32;

            return (r * 256.0 + g + b / 256.0) - 32768.0;
        }
    }

    0.0
}

static TOKIO_RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Failed to create tokio runtime"));
fn update() {}

async fn build_mesh(
    normal: Dir3,
    chunk_translation: Vec3,
    client: Client,
    semaphore: Arc<Semaphore>,
    cache: TileCache,
) -> Option<Mesh> {
    let n = 2.0_f32.powf(ZOOM as f32);
    let required_tiles = calculate_required_tiles_for_chunk(chunk_translation, normal, n);
    ensure_tiles_loaded(
        &client,
        Arc::clone(&semaphore),
        Arc::clone(&cache),
        required_tiles,
    )
    .await;
    let mut earth_mesh = Mesh::from(
        Plane3d::default()
            .mesh()
            .size(SIZE / SUBDIV as f32, SIZE / SUBDIV as f32)
            .normal(normal)
            .subdivisions(SUBDIV_PER_TILE),
    )
    .translated_by(chunk_translation);

    // make the planes a sphere
    if let bevy::mesh::VertexAttributeValues::Float32x3(positions) = earth_mesh
        .try_attribute_mut(Mesh::ATTRIBUTE_POSITION)
        .unwrap()
    {
        for pos in positions.iter_mut() {
            let even_spaced_pos = to_sphere_pos(&pos);
            *pos = (even_spaced_pos).to_array();

            let coord = pos_to_coord(*pos);

            let factor = 1.0 + (0.0000001 * get_height_at_coord(coord, ZOOM, &cache));
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

fn calculate_required_tiles_for_chunk(
    chunk_translation: Vec3,
    normal: Dir3,
    n: f32,
) -> Vec<(u8, u32, u32)> {
    let mut unique_tiles = HashSet::new();

    let chunk_size = SIZE / SUBDIV as f32;
    let rotation = Quat::from_rotation_arc(Vec3::Y, *normal);

    let steps = SUBDIV_PER_TILE + 1;

    for i in 0..steps {
        for j in 0..steps {
            // Calculate percentage across the face (-0.5 to 0.5)
            let pct_x = (i as f32 / (steps - 1) as f32) - 0.5;
            let pct_z = (j as f32 / (steps - 1) as f32) - 0.5;

            // Map to local plane space
            let local_pos = Vec3::new(pct_x * chunk_size, 0.0, pct_z * chunk_size);

            // Transform to world spherical space
            let world_plane_pos = rotation * local_pos + chunk_translation;
            let even_spaced_pos = to_sphere_pos(&world_plane_pos.to_array());
            let coord = pos_to_coord(even_spaced_pos.to_array());

            // Get the exact tile for this specific vertex
            let (tx, ty) = coord_to_tile(coord, n);

            unique_tiles.insert((ZOOM, tx, ty));
        }
    }

    // Safely protect tile boundaries (Web Mercator limits)
    let max_tile_limit = (2.0_f32.powi(ZOOM as i32) as u32).saturating_sub(1);

    let mut final_tiles = Vec::new();
    for (z, tx, ty) in unique_tiles {
        // Clamp tiles to valid map ranges just in case of edge precision issues
        let clamped_x = tx.min(max_tile_limit);
        let clamped_y = ty.min(max_tile_limit);
        final_tiles.push((z, clamped_x, clamped_y));
    }

    final_tiles
}

fn pos_to_coord(pos: [f32; 3]) -> Coord {
    let distance_h = (pos[0].powi(2) + pos[2].powi(2)).sqrt();

    let bearing = pos[0].atan2(pos[2]).to_degrees();

    let elevation = pos[1]
        .atan2(distance_h)
        .to_degrees()
        .clamp(-85.05113, 85.05113);

    let coord = Coord {
        lat: elevation,
        long: bearing,
    };
    coord
}

async fn get_tile(
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
                        ),
                        cell_coord,
                    ))
                    .id();

                commands.entity(*big_space).add_child(chunk);
            }
            commands.entity(entity).remove::<SpawnTerrain>();
        }
    }
}

// https://github.com/evroon/bevy-open-world/blob/master/crates/bevy-terrain/src/camera.rs
pub fn rotate_sun(
    mut suns: Query<&mut Transform, With<DirectionalLight>>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    let mut sun_vert_rot_factor = 0.0;
    let mut sun_hor_rot_factor = 0.0;

    if keys.pressed(KeyCode::KeyH) {
        sun_vert_rot_factor -= 0.1;
    }
    if keys.pressed(KeyCode::KeyJ) {
        sun_vert_rot_factor += 0.1;
    }
    if keys.pressed(KeyCode::KeyK) {
        sun_hor_rot_factor -= 0.2;
    }
    if keys.pressed(KeyCode::KeyL) {
        sun_hor_rot_factor += 0.2;
    }

    suns.iter_mut().for_each(|mut tf| {
        tf.rotate_x(time.delta_secs() * PI * sun_vert_rot_factor);
        tf.rotate_y(time.delta_secs() * PI * sun_hor_rot_factor)
    });
}
