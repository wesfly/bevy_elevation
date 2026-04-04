use avian3d::{
    PhysicsPlugins,
    prelude::{Collider, RigidBody},
};
use bevy::{
    camera::Exposure,
    camera_controller::free_camera::{FreeCamera, FreeCameraPlugin},
    core_pipeline::tonemapping::Tonemapping,
    dev_tools::fps_overlay::FpsOverlayPlugin,
    light::{AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder, light_consts::lux},
    pbr::{Atmosphere, AtmosphereSettings, ScatteringMedium},
    post_process::bloom::Bloom,
    prelude::*,
    render::view::Hdr,
};
use big_space::prelude::*;

#[cfg(debug_assertions)]
use avian3d::prelude::PhysicsDebugPlugin;

fn main() {
    App::new()
        .insert_resource(ClearColor(Color::LinearRgba(LinearRgba {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        })))
        .add_plugins((
            DefaultPlugins,
            FreeCameraPlugin,
            PhysicsPlugins::default(),
            // #[cfg(debug_assertions)]
            // PhysicsDebugPlugin,
        ))
        .add_systems(Startup, setup)
        .add_plugins(FpsOverlayPlugin::default())
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
) {
    let cascade = CascadeShadowConfigBuilder {
        maximum_distance: 50000.0,
        ..Default::default()
    }
    .build();

    commands.spawn((
        DirectionalLight {
            shadows_enabled: true,
            illuminance: lux::RAW_SUNLIGHT,
            ..default()
        },
        cascade,
        Transform::from_xyz(1.0, 1.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((
        Camera3d::default(),
        Atmosphere::earthlike(scattering_mediums.add(ScatteringMedium::default())),
        AtmosphereEnvironmentMapLight::default(),
        AtmosphereSettings::default(),
        Exposure::SUNLIGHT,
        Tonemapping::AgX,
        Bloom::NATURAL,
        FreeCamera {
            run_speed: 1000000.0,
            walk_speed: 10000.0,
            ..default()
        },
        Hdr,
        FloatingOrigin,
    ));

    let normals = vec![
        Dir3::X,
        Dir3::Y,
        Dir3::Z,
        Dir3::NEG_X,
        Dir3::NEG_Y,
        Dir3::NEG_Z,
    ];

    for normal in normals {
        spawn_face(&mut commands, &mut meshes, &mut materials, normal);
    }
}

pub fn spawn_face(
    commands: &mut Commands,
    meshes: &mut ResMut<Assets<Mesh>>,
    materials: &mut ResMut<Assets<StandardMaterial>>,
    normal: Dir3,
) {
    const SIZE: f32 = 2.0;
    const CHUNKS: u8 = 49;
    const SUBDIV: u8 = CHUNKS.isqrt();
    const RADIUS: f32 = 6_371_000.0;

    for i in 0..CHUNKS {
        let a = (i % SUBDIV) as f32 * (SIZE / SUBDIV as f32) - (SIZE / SUBDIV as f32 * 3.0);
        let b = (i / SUBDIV) as f32 * (SIZE / SUBDIV as f32) - (SIZE / SUBDIV as f32 * 3.0);

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

        // Translate this to be normalized properly
        let chunk_translation = Vec3 {
            x: normal.x,
            y: normal.y,
            z: normal.z,
        } + translation_per_chunk;

        let mut earth_mesh_even = Mesh::from(
            Plane3d::default()
                .mesh()
                .size(SIZE / SUBDIV as f32, SIZE / SUBDIV as f32)
                .normal(normal)
                .subdivisions(8),
        )
        .translated_by(chunk_translation);

        if let bevy::mesh::VertexAttributeValues::Float32x3(positions) = earth_mesh_even
            .try_attribute_mut(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
        {
            for pos in positions.iter_mut() {
                let p = Vec3 {
                    x: pos[0],
                    y: pos[1],
                    z: pos[2],
                };

                // Even spacing of vertices on sphere
                let x = p.x
                    * (1.0 - (p.y.powi(2) + p.z.powi(2)) / 2.0 + (p.y.powi(2) * p.z.powi(2) / 3.0))
                        .sqrt();
                let y = p.y
                    * (1.0 - (p.z.powi(2) + p.x.powi(2)) / 2.0 + (p.z.powi(2) * p.x.powi(2) / 3.0))
                        .sqrt();
                let z = p.z
                    * (1.0 - (p.x.powi(2) + p.y.powi(2)) / 2.0 + (p.x.powi(2) * p.y.powi(2) / 3.0))
                        .sqrt();
                let even_spaced_pos = Vec3::new(x, y, z);

                *pos = (even_spaced_pos).to_array();
            }
        }

        earth_mesh_even.compute_normals();
        commands.spawn((
            Collider::trimesh_from_mesh(&earth_mesh_even).unwrap(),
            Mesh3d(meshes.add(earth_mesh_even)),
            RigidBody::Static,
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: bevy::color::palettes::css::GREEN.into(),
                perceptual_roughness: 1.0,
                ..Default::default()
            })),
            Transform::from_translation(Vec3 {
                x: 0.0,
                y: -RADIUS,
                z: 0.0,
            })
            .with_scale(Vec3 {
                x: RADIUS,
                y: RADIUS,
                z: RADIUS,
            }),
        ));
    }
}
