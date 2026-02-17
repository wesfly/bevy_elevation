use avian3d::PhysicsPlugins;
use avian3d::prelude::{Collider, RigidBody};
use bevy::camera_controller::free_camera::{FreeCamera, FreeCameraPlugin};
use bevy::dev_tools::fps_overlay::FpsOverlayPlugin;
use bevy::prelude::*;

// #[cfg(debug_assertions)]
// use avian3d::debug_render::PhysicsDebugPlugin;

fn main() {
    App::new()
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
) {
    commands.spawn((
        DirectionalLight::default(),
        Transform::from_xyz(1.0, 1.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((Camera3d::default(), FreeCamera::default()));

    let normals = vec![
        Dir3::X,
        Dir3::Y,
        Dir3::Z,
        Dir3::NEG_X,
        Dir3::NEG_Y,
        Dir3::NEG_Z,
    ];

    for normal in normals {
        spawn_chunk(&mut commands, &mut meshes, &mut materials, normal);
    }
}

pub fn spawn_chunk(
    commands: &mut Commands,
    meshes: &mut ResMut<Assets<Mesh>>,
    materials: &mut ResMut<Assets<StandardMaterial>>,
    normal: Dir3,
) {
    const SIZE: f32 = 0.2;
    const RADIUS: f32 = 5.0;

    let mut obj_mesh = Mesh::from(
        Plane3d::default()
            .mesh()
            .size(SIZE * 2.0, SIZE * 2.0)
            .normal(normal)
            .subdivisions(128),
    );

    if let bevy::mesh::VertexAttributeValues::Float32x3(positions) = obj_mesh
        .try_attribute_mut(Mesh::ATTRIBUTE_POSITION)
        .unwrap()
    {
        for pos in positions.iter_mut() {
            // Positioning the plane to normalize it
            let position = normal.as_vec3().to_array();
            pos[0] += SIZE * position[0];
            pos[1] += SIZE * position[1];
            pos[2] += SIZE * position[2];

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

            *pos = (even_spaced_pos.normalize() * RADIUS).to_array();
        }
    }

    obj_mesh.compute_normals();
    commands.spawn((
        Collider::trimesh_from_mesh(&obj_mesh).unwrap(),
        Mesh3d(meshes.add(obj_mesh)),
        RigidBody::Static,
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: bevy::color::palettes::css::GREEN.into(),
            perceptual_roughness: 1.0,
            ..Default::default()
        })),
        Transform::from_translation(Vec3 {
            x: 0.0,
            y: -0.5 * RADIUS,
            z: 0.0,
        }),
    ));
}
