struct CameraUniform {
    proj: mat4x4<f32>;
    view: mat4x4<f32>;
    rotation_only_view: mat4x4<f32>;
    near_plane_distance: f32;
    far_plane_distance: f32;
};
[[group(1), binding(0)]]
var<uniform> camera: CameraUniform;

struct LightPositionUniform {
    value: vec4<f32>;
};
[[group(1), binding(1)]]
var<uniform> light_position: LightPositionUniform;

struct VertexInput {
    [[location(0)]] object_position: vec3<f32>;
    [[location(1)]] object_normal: vec3<f32>;
    [[location(2)]] object_tex_coords: vec2<f32>;
    [[location(3)]] object_tangent: vec3<f32>;
    [[location(4)]] object_bitangent: vec3<f32>;
};

struct VertexOutput {
    [[builtin(position)]] clip_position: vec4<f32>;
    [[location(0)]] world_position: vec3<f32>;
};

let pi: f32 = 3.141592653589793;
let two_pi: f32 = 6.283185307179586;

// in radians
fn angle_modulo(angle: f32) -> f32 {
  return (angle + two_pi) % two_pi;
}

fn latlng_to_uv(latitude: f32, longitude: f32) -> vec2<f32> {
  return vec2<f32>(
      angle_modulo(longitude) / two_pi,
      -(latitude - (pi / 2.0)) / pi, 
  );
}

[[stage(vertex)]]
fn vs_main(
    vshader_input: VertexInput,
) -> VertexOutput {
    var out: VertexOutput;
    let clip_position = camera.proj * camera.rotation_only_view * vec4<f32>(vshader_input.object_position, 1.0);
    out.clip_position = vec4<f32>(clip_position.x, clip_position.y, 0.0, clip_position.w);
    out.world_position = vshader_input.object_position;
    return out;
}

[[group(0), binding(0)]]
var r_texture: texture_2d<f32>;

[[group(0), binding(1)]]
var r_sampler: sampler;

[[stage(fragment)]]
fn fs_main(in: VertexOutput) -> [[location(0)]] vec4<f32> {
    let latitude = atan2(
      in.world_position.y, 
      sqrt(in.world_position.x * in.world_position.x + in.world_position.z * in.world_position.z)
    );
    let longitude = atan2(in.world_position.z, in.world_position.x);
    
    return textureSample(r_texture, r_sampler, latlng_to_uv(latitude, longitude));
}