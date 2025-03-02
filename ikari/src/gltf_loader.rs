use crate::asset_loader::SceneAssetLoadParams;
use crate::collisions::Aabb;
use crate::file_manager::GameFilePath;
use crate::mesh::*;
use crate::renderer::*;
use crate::sampler_cache::*;
use crate::scene::*;
use crate::texture::RawImage;
#[cfg(not(target_arch = "wasm32"))]
use crate::texture_compression::*;
use crate::transform::*;

use std::collections::{hash_map::Entry, HashMap};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

use anyhow::{bail, Result};
use approx::abs_diff_eq;
use glam::f32::{Mat4, Vec2, Vec3, Vec4};

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct ChannelPropertyStr<'a>(&'a str);

impl From<gltf::animation::Property> for ChannelPropertyStr<'_> {
    fn from(prop: gltf::animation::Property) -> Self {
        Self(match prop {
            gltf::animation::Property::Translation => "Translation",
            gltf::animation::Property::Scale => "Scale",
            gltf::animation::Property::Rotation => "Rotation",
            gltf::animation::Property::MorphTargetWeights => "MorphTargetWeights",
        })
    }
}

#[profiling::function]
pub async fn build_scene(
    (document, buffers, images): (
        &gltf::Document,
        &Vec<gltf::buffer::Data>,
        &Vec<gltf::image::Data>,
    ),
    params: SceneAssetLoadParams,
) -> Result<(Scene, BindableSceneData)> {
    let scene_index = document
        .default_scene()
        .map(|scene| scene.index())
        .unwrap_or(0);

    let materials: Vec<_> = document.materials().collect();
    let material_count = materials.len();

    let textures = get_textures(document, images, materials, &params.path).await?;

    // gltf node index -> gltf parent node index
    let gltf_parent_index_map: HashMap<usize, usize> = document
        .nodes()
        .flat_map(|parent_node| {
            let parent_node_index = parent_node.index();
            parent_node
                .children()
                .map(move |child_node| (child_node.index(), parent_node_index))
        })
        .collect();

    let scene_nodes: Vec<_> = get_full_node_list(
        document
            .scenes()
            .find(|scene| scene.index() == scene_index)
            .ok_or_else(|| anyhow::anyhow!("Expected scene with index: {:?}", scene_index))?,
    );

    // mesh index -> node indices
    let mut mesh_node_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for node in &scene_nodes {
        if node.mesh().is_none() {
            continue;
        }
        let mesh_index = node.mesh().unwrap().index();
        match mesh_node_map.entry(mesh_index) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().push(node.index());
            }
            Entry::Vacant(entry) => {
                entry.insert(vec![node.index()]);
            }
        }
    }

    let meshes: Vec<_> = document.meshes().collect();

    let mut supported_meshes = vec![];
    let mut supported_primitives = vec![];

    for mesh in meshes.iter() {
        for primitive in mesh.primitives() {
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                log::warn!(
                    "{:?}: Primitive mode {:?} is not currently supported. Primitive {:?} of mesh {:?} will be skipped.",
                    params.path.relative_path,
                    primitive.mode(),
                    primitive.index(),
                    mesh.index(),
                );
                continue;
            }

            if primitive.material().alpha_mode() == gltf::material::AlphaMode::Blend {
                log::warn!(
                    "{:?}: Loading gltf materials in alpha blending mode is not current supported. Material {:?} will be rendered as opaque.",
                    params.path.relative_path,
                    primitive.material().index()
                );
            }

            supported_primitives.push(primitive);
        }

        supported_meshes.push((mesh, supported_primitives.clone()));
        supported_primitives.clear();
    }

    let mut bindable_meshes: Vec<BindableGeometryBuffers> =
        Vec::with_capacity(supported_meshes.len());
    let mut bindable_wireframe_meshes: Vec<BindableWireframeMesh> =
        Vec::with_capacity(supported_meshes.len());
    let mut bindable_pbr_materials: Vec<BindablePbrMaterial> = Vec::with_capacity(material_count);

    // collect a list of visuals to be created for each gltf node
    // gltf node index -> Vec<(bindable mesh index, bindable material index)>
    let mut node_visual_map: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();

    {
        profiling::scope!("meshes");

        for (_gltf_mesh_index, (mesh, primitives)) in supported_meshes.iter().enumerate() {
            for primitive in primitives.iter() {
                let (geometry, wireframe_indices) = build_geometry(primitive, buffers)?;
                bindable_meshes.push(geometry);
                let mesh_index = bindable_meshes.len() - 1;

                if params.generate_wireframe_meshes {
                    bindable_wireframe_meshes.push(BindableWireframeMesh {
                        source_mesh_index: mesh_index,
                        indices: wireframe_indices,
                    });
                }

                bindable_pbr_materials.push(BindablePbrMaterial {
                    textures: get_indexed_pbr_material(&primitive.material()),
                    dynamic_pbr_params: get_dynamic_pbr_params(&primitive.material()),
                });
                let pbr_material_index = bindable_pbr_materials.len() - 1;

                if let Some(gltf_node_indices) = mesh_node_map.get(&mesh.index()) {
                    for gltf_node_index in gltf_node_indices {
                        let node_visuals =
                            node_visual_map.entry(*gltf_node_index).or_insert(vec![]);
                        node_visuals.push((mesh_index, pbr_material_index));
                    }
                }
            }
        }
    }

    // it is important that the node indices from the gltf document are preserved
    // for any of the other stuff that refers to the nodes by index such as the animations
    let mut nodes: Vec<IndexedGameNodeDesc> = vec![];
    for gltf_node in document.nodes() {
        // 'parent node', corresponds directly to gltf node
        nodes.push(IndexedGameNodeDesc {
            transform: crate::transform::Transform::from(gltf_node.transform()),
            skin_index: gltf_node.skin().map(|skin| skin.index()),
            visual: None, // will be added later
            name: gltf_node.name().map(|name| name.to_string()),
            parent_index: gltf_parent_index_map.get(&gltf_node.index()).copied(),
        });
    }

    for gltf_node in document.nodes() {
        if let Some(visuals) = node_visual_map.get(&gltf_node.index()) {
            for (i, (mesh_index, pbr_material_index)) in visuals.iter().enumerate() {
                let visual = GameNodeVisual::from_mesh_mat(
                    *mesh_index,
                    Material::Pbr {
                        binded_material_index: *pbr_material_index,
                        dynamic_pbr_params: None,
                    },
                );

                if visuals.len() == 1 {
                    // don't bother adding 'auto-child' nodes, just put the visual on the 'parent' node.
                    // skinning breaks without this optimization 🙃
                    nodes[gltf_node.index()].visual = Some(visual);
                } else {
                    // child nodes which don't exist as gltf nodes but are used to display the visuals of the above 'parent node'
                    nodes.push(IndexedGameNodeDesc {
                        transform: Default::default(),
                        skin_index: None,
                        visual: Some(visual),
                        name: gltf_node
                            .name()
                            .map(|name| format!("{name} (auto-child {i})",)),
                        parent_index: Some(gltf_node.index()),
                    });
                }
            }
        }
    }

    let animations = get_animations(document, buffers)?;

    let skins: Vec<_> = {
        profiling::scope!("skins");
        document
            .skins()
            .enumerate()
            .map(|(skin_index, skin)| {
                let bone_node_indices: Vec<_> = skin.joints().map(|joint| joint.index()).collect();
                let bone_inverse_bind_matrices: Vec<_> = skin
                    .inverse_bind_matrices()
                    .map(|accessor| {
                        let data_type = accessor.data_type();
                        let dimensions = accessor.dimensions();
                        if dimensions != gltf::accessor::Dimensions::Mat4 {
                            bail!("Expected mat4 data but found: {:?}", dimensions);
                        }
                        if data_type != gltf::accessor::DataType::F32 {
                            bail!("Expected f32 data but found: {:?}", data_type);
                        }
                        let matrices_u8 = get_buffer_slice_from_accessor(accessor, buffers);
                        Ok(bytemuck::cast_slice::<_, [[f32; 4]; 4]>(matrices_u8)
                            .to_vec()
                            .iter()
                            .map(Mat4::from_cols_array_2d)
                            .collect())
                    })
                    .transpose()?
                    .unwrap_or_else(|| {
                        (0..bone_node_indices.len())
                            .map(|_| Mat4::IDENTITY)
                            .collect()
                    });

                let skeleton_skin_node = nodes
                    .iter()
                    .find(|node| node.skin_index == Some(skin_index))
                    .unwrap();
                let skeleton_mesh_index = skeleton_skin_node
                    .visual
                    .as_ref()
                    .expect("Skeleton skin node should have a mesh")
                    .mesh_index;
                let skeleton_mesh_vertices = &bindable_meshes[skeleton_mesh_index].vertices;

                let bone_bounding_box_transforms: Vec<_> = (0..bone_inverse_bind_matrices.len())
                    .map(|bone_index| {
                        let bone_inv_bind_matrix = bone_inverse_bind_matrices[bone_index];
                        let vertex_weight_threshold = 0.5f32;
                        let vertex_positions_for_node = skeleton_mesh_vertices
                            .iter()
                            .filter(|vertex| {
                                vertex
                                    .bone_indices
                                    .iter()
                                    .zip(vertex.bone_weights.iter())
                                    .any(|(v_bone_index, v_bone_weight)| {
                                        *v_bone_index as usize == bone_index
                                            && *v_bone_weight > vertex_weight_threshold
                                    })
                            })
                            .map(|vertex| {
                                bone_inv_bind_matrix.transform_point3(Vec3::from(vertex.position))
                            });
                        match Aabb::make_from_points(vertex_positions_for_node) {
                            Some(aabb) => TransformBuilder::new()
                                .scale((aabb.max - aabb.min) / 2.0)
                                .position(aabb.center())
                                .build(),
                            None => TransformBuilder::new()
                                .scale(Vec3::new(0.0, 0.0, 0.0))
                                .build(),
                        }
                    })
                    .collect();

                anyhow::Ok(IndexedSkin {
                    bone_inverse_bind_matrices,
                    bone_node_indices,
                    bone_bounding_box_transforms,
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let bindable_scene_data = BindableSceneData {
        bindable_meshes,
        bindable_wireframe_meshes,
        bindable_pbr_materials,
        textures,
    };

    log::debug!("Scene loaded ({:?}):", params.path.relative_path);

    log::debug!("  - node count: {:?}", nodes.len());
    log::debug!("  - skin count: {:?}", skins.len());
    log::debug!("  - animation count: {:?}", animations.len());
    log::debug!("  Render buffers:");
    log::debug!(
        "    - Mesh count: {:?}",
        bindable_scene_data.bindable_meshes.len()
    );
    log::debug!(
        "    - Wireframe mesh count: {:?}",
        bindable_scene_data.bindable_wireframe_meshes.len()
    );
    log::debug!(
        "    - PBR material count: {:?}",
        bindable_scene_data.bindable_pbr_materials.len()
    );
    log::debug!(
        "    - Texture count: {:?}",
        bindable_scene_data.textures.len()
    );

    let scene = Scene::new(nodes, skins, animations);

    Ok((scene, bindable_scene_data))
}

#[profiling::function]
async fn get_textures(
    document: &gltf::Document,
    images: &[gltf::image::Data],
    materials: Vec<gltf::Material<'_>>,
    gltf_path: &GameFilePath,
) -> Result<Vec<BindableTexture>> {
    let mut textures: Vec<BindableTexture> = vec![];
    for texture in document.textures() {
        let source_image_index = texture.source().index();

        let is_srgb = materials.iter().any(|material| {
            [
                material.emissive_texture(),
                material.pbr_metallic_roughness().base_color_texture(),
            ]
            .iter()
            .flatten()
            .any(|texture_info| texture_info.texture().index() == texture.index())
        });

        let is_normal_map = !is_srgb
            && materials.iter().any(|material| {
                material.normal_texture().is_some()
                    && material.normal_texture().unwrap().texture().index() == texture.index()
            });

        let (raw_image, texture_format) = get_raw_image_and_format(
            gltf_path,
            &texture,
            &images[source_image_index],
            is_srgb,
            is_normal_map,
        )
        .await?;

        let gltf_sampler = texture.sampler();
        let default_sampler = SamplerDescriptor {
            min_filter: wgpu::FilterMode::Linear,
            mag_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        };
        let address_mode_u = sampler_wrapping_mode_to_wgpu(gltf_sampler.wrap_s());
        let address_mode_v = sampler_wrapping_mode_to_wgpu(gltf_sampler.wrap_t());
        let mag_filter = gltf_sampler
            .mag_filter()
            .map(|gltf_mag_filter| match gltf_mag_filter {
                gltf::texture::MagFilter::Nearest => wgpu::FilterMode::Nearest,
                gltf::texture::MagFilter::Linear => wgpu::FilterMode::Linear,
            })
            .unwrap_or(default_sampler.mag_filter);

        let (min_filter, mipmap_filter) = gltf_sampler
            .min_filter()
            .map(|gltf_min_filter| match gltf_min_filter {
                gltf::texture::MinFilter::Nearest => {
                    (wgpu::FilterMode::Nearest, default_sampler.mipmap_filter)
                }
                gltf::texture::MinFilter::Linear => {
                    (wgpu::FilterMode::Linear, default_sampler.mipmap_filter)
                }
                gltf::texture::MinFilter::NearestMipmapNearest => {
                    (wgpu::FilterMode::Nearest, wgpu::FilterMode::Nearest)
                }
                gltf::texture::MinFilter::LinearMipmapNearest => {
                    (wgpu::FilterMode::Linear, wgpu::FilterMode::Nearest)
                }
                gltf::texture::MinFilter::NearestMipmapLinear => {
                    (wgpu::FilterMode::Nearest, wgpu::FilterMode::Linear)
                }
                gltf::texture::MinFilter::LinearMipmapLinear => {
                    (wgpu::FilterMode::Linear, wgpu::FilterMode::Linear)
                }
            })
            .unwrap_or((default_sampler.min_filter, default_sampler.mipmap_filter));

        textures.push(BindableTexture {
            raw_image,
            name: texture.name().map(|name| name.to_string()),
            format: texture_format.into(),
            sampler_descriptor: SamplerDescriptor {
                address_mode_u,
                address_mode_v,
                mag_filter,
                min_filter,
                mipmap_filter,
                ..Default::default()
            },
        });
    }
    Ok(textures)
}

pub fn get_animations(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
) -> Result<Vec<IndexedAnimation>> {
    document
        .animations()
        .map(|animation| {
            let channel_timings: Vec<_> = animation
                .channels()
                .map(|channel| get_keyframe_times(&channel.sampler(), buffers))
                .collect::<Result<Vec<_>, _>>()?;
            let length_seconds = *channel_timings
                .iter()
                .map(|keyframe_times| keyframe_times.last().unwrap())
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap();
            let channels: Vec<_> = animation
                .channels()
                .enumerate()
                .map(|(channel_index, channel)| {
                    validate_channel_data_type(&channel)?;
                    let sampler = channel.sampler();
                    let accessor = sampler.output();
                    anyhow::Ok(IndexedChannel {
                        node_index: channel.target().node().index(),
                        property: channel.target().property(),
                        interpolation_type: sampler.interpolation(),
                        keyframe_timings: channel_timings[channel_index].clone(),
                        keyframe_values_u8: get_buffer_slice_from_accessor(accessor, buffers)
                            .to_vec(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            anyhow::Ok(IndexedAnimation {
                name: animation.name().map(String::from),
                length_seconds,
                channels,
            })
        })
        .collect::<Result<Vec<_>, _>>()
}

fn validate_channel_data_type(channel: &gltf::animation::Channel) -> Result<()> {
    let accessor = channel.sampler().output();
    let data_type = accessor.data_type();
    let dimensions = accessor.dimensions();
    match channel.target().property() {
        gltf::animation::Property::Translation | gltf::animation::Property::Scale => {
            if dimensions != gltf::accessor::Dimensions::Vec3 {
                bail!("Expected vec3 data but found: {:?}", dimensions);
            }
            if data_type != gltf::accessor::DataType::F32 {
                bail!("Expected f32 data but found: {:?}", data_type);
            }
        }
        gltf::animation::Property::Rotation => {
            if dimensions != gltf::accessor::Dimensions::Vec4 {
                bail!("Expected vec4 data but found: {:?}", dimensions);
            }
            if data_type != gltf::accessor::DataType::F32 {
                bail!("Expected f32 data but found: {:?}", data_type);
            }
        }
        gltf::animation::Property::MorphTargetWeights => {
            bail!("MorphTargetWeights not supported")
        }
    };
    Ok(())
}

fn get_keyframe_times(
    sampler: &gltf::animation::Sampler,
    buffers: &[gltf::buffer::Data],
) -> Result<Vec<f32>> {
    let keyframe_times = {
        let accessor = sampler.input();
        let data_type = accessor.data_type();
        let dimensions = accessor.dimensions();
        if dimensions != gltf::accessor::Dimensions::Scalar {
            bail!("Expected scalar data but found: {:?}", dimensions);
        }
        if data_type != gltf::accessor::DataType::F32 {
            bail!("Expected f32 data but found: {:?}", data_type);
        }
        let result_u8 = get_buffer_slice_from_accessor(accessor, buffers);
        bytemuck::cast_slice::<_, f32>(result_u8).to_vec()
    };

    Ok(keyframe_times)
}

#[cfg(not(target_arch = "wasm32"))]
#[profiling::function]
async fn get_raw_image_and_format(
    gltf_path: &GameFilePath,
    texture: &gltf::Texture<'_>,
    image_data: &gltf::image::Data,
    is_srgb: bool,
    is_normal_map: bool,
) -> Result<(RawImage, wgpu::TextureFormat)> {
    use crate::file_manager::FileManager;

    let compressed_image = match texture.source().source() {
        gltf::image::Source::Uri { uri, .. } => {
            let compressed_texture_path = texture_path_to_compressed_path(&{
                let mut texture_path = gltf_path.clone();
                texture_path.relative_path = gltf_path
                    .relative_path
                    .parent()
                    .unwrap()
                    .join(PathBuf::from(uri));
                texture_path
            });
            if compressed_texture_path.resolve().try_exists()? {
                let texture_compressor = TextureCompressor;
                let texture_bytes = FileManager::read(&compressed_texture_path).await?;
                Some(texture_compressor.transcode_image(&texture_bytes, is_normal_map)?)
            } else {
                None
            }
        }
        gltf::image::Source::View { .. } => None,
    };

    let mip_count = compressed_image
        .as_ref()
        .map(|compressed_image| compressed_image.raw_image.mip_count)
        .unwrap_or(1);
    let (image_width, image_height) = compressed_image
        .as_ref()
        .map(|data| (data.raw_image.width, data.raw_image.height))
        .unwrap_or((image_data.width, image_data.height));

    let (image_pixels, texture_format) = match compressed_image {
        Some(compressed_image) => {
            let format_wgpu = compressed_image.format_wgpu(is_srgb);
            (compressed_image.raw_image.raw, format_wgpu)
        }
        None => get_uncompressed_image_and_format(image_data, is_srgb)?,
    };

    Ok((
        RawImage {
            raw: image_pixels,
            width: image_width,
            height: image_height,
            depth: 1,
            mip_count,
        },
        texture_format,
    ))
}

#[cfg(target_arch = "wasm32")]
async fn get_raw_image_and_format(
    _gltf_path: &GameFilePath,
    _texture: &gltf::Texture<'_>,
    image_data: &gltf::image::Data,
    is_srgb: bool,
    _is_normal_map: bool,
) -> Result<(RawImage, wgpu::TextureFormat)> {
    let (image_pixels, texture_format) = get_uncompressed_image_and_format(image_data, is_srgb)?;
    Ok((
        RawImage {
            raw: image_pixels,
            width: image_data.width,
            height: image_data.height,
            depth: 1,
            mip_count: 1,
        },
        texture_format,
    ))
}

fn get_uncompressed_image_and_format(
    image_data: &gltf::image::Data,
    srgb: bool,
) -> Result<(Vec<u8>, wgpu::TextureFormat)> {
    let image_pixels = &image_data.pixels;
    let (image_pixels, texture_format) = match (image_data.format, srgb) {
        (gltf::image::Format::R8G8B8, srgb) => {
            let image = image::RgbImage::from_raw(
                image_data.width,
                image_data.height,
                image_pixels.to_vec(),
            )
            .ok_or_else(|| anyhow::anyhow!("Failed to decode R8G8B8 image"))?;
            let image_pixels_conv = image::DynamicImage::ImageRgb8(image).to_rgba8().to_vec();
            anyhow::Ok((
                image_pixels_conv,
                if srgb {
                    wgpu::TextureFormat::Rgba8UnormSrgb
                } else {
                    wgpu::TextureFormat::Rgba8Unorm
                },
            ))
        }
        (gltf::image::Format::R8G8, true) => {
            // srgb is true meaning this is a color image, so the red channel is luma and g is alpha
            Ok((
                image_pixels
                    .chunks(2)
                    .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                    .collect(),
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ))
        }
        _ => {
            let texture_format = texture_format_to_wgpu(image_data.format, srgb)?;
            Ok((image_pixels.to_vec(), texture_format))
        }
    }?;
    Ok((image_pixels, texture_format))
}

fn texture_format_to_wgpu(format: gltf::image::Format, srgb: bool) -> Result<wgpu::TextureFormat> {
    match (format, srgb) {
        (gltf::image::Format::R8, false) => Ok(wgpu::TextureFormat::R8Unorm),
        (gltf::image::Format::R8G8, false) => Ok(wgpu::TextureFormat::Rg8Unorm),
        (gltf::image::Format::R8G8B8A8, false) => Ok(wgpu::TextureFormat::Rgba8Unorm),
        (gltf::image::Format::R8G8B8A8, true) => Ok(wgpu::TextureFormat::Rgba8UnormSrgb),
        (gltf::image::Format::R16, false) => Ok(wgpu::TextureFormat::R16Unorm),
        (gltf::image::Format::R16G16, false) => Ok(wgpu::TextureFormat::Rg16Unorm),
        (gltf::image::Format::R16G16B16A16, false) => Ok(wgpu::TextureFormat::Rgba16Unorm),
        _ => bail!(
            "Unsupported texture format combo: {:?}, srgb={:?}",
            format,
            srgb
        ),
    }
}

fn sampler_wrapping_mode_to_wgpu(wrapping_mode: gltf::texture::WrappingMode) -> wgpu::AddressMode {
    match wrapping_mode {
        gltf::texture::WrappingMode::ClampToEdge => wgpu::AddressMode::ClampToEdge,
        gltf::texture::WrappingMode::MirroredRepeat => wgpu::AddressMode::MirrorRepeat,
        gltf::texture::WrappingMode::Repeat => wgpu::AddressMode::Repeat,
    }
}

fn get_full_node_list(scene: gltf::scene::Scene) -> Vec<gltf::scene::Node> {
    scene
        .nodes()
        .flat_map(|node| get_full_node_list_impl(node, Vec::new()))
        .collect()
}

fn get_full_node_list_impl<'a>(
    node: gltf::scene::Node<'a>,
    acc: Vec<gltf::scene::Node<'a>>,
) -> Vec<gltf::scene::Node<'a>> {
    let acc_with_self: Vec<_> = acc.iter().chain([node.clone()].iter()).cloned().collect();
    if node.children().count() == 0 {
        acc_with_self
    } else {
        node.children()
            .flat_map(|child| get_full_node_list_impl(child, acc_with_self.clone()))
            .collect()
    }
}

fn get_indexed_pbr_material(material: &gltf::material::Material) -> IndexedPbrTextures {
    let pbr_info = material.pbr_metallic_roughness();

    let get_texture_index =
        |texture: Option<gltf::texture::Texture>| texture.map(|texture| texture.index());

    IndexedPbrTextures {
        base_color: get_texture_index(pbr_info.base_color_texture().map(|info| info.texture())),
        normal: get_texture_index(material.normal_texture().map(|info| info.texture())),
        emissive: get_texture_index(material.emissive_texture().map(|info| info.texture())),
        ambient_occlusion: get_texture_index(
            material.occlusion_texture().map(|info| info.texture()),
        ),
        metallic_roughness: get_texture_index(
            pbr_info
                .metallic_roughness_texture()
                .map(|info| info.texture()),
        ),
    }
}

fn get_dynamic_pbr_params(material: &gltf::material::Material) -> DynamicPbrParams {
    let pbr_info = material.pbr_metallic_roughness();

    DynamicPbrParams {
        base_color_factor: Vec4::from(pbr_info.base_color_factor()),
        emissive_factor: Vec3::from(material.emissive_factor()),
        metallic_factor: pbr_info.metallic_factor(),
        roughness_factor: pbr_info.roughness_factor(),
        normal_scale: material
            .normal_texture()
            .map(|info| info.scale())
            .unwrap_or(DynamicPbrParams::default().normal_scale),
        occlusion_strength: material
            .occlusion_texture()
            .map(|info| info.strength())
            .unwrap_or(DynamicPbrParams::default().occlusion_strength),
        alpha_cutoff: match material.alpha_mode() {
            gltf::material::AlphaMode::Mask => material.alpha_cutoff().unwrap_or(0.5),
            _ => DynamicPbrParams::default().alpha_cutoff,
        },
    }
}

pub fn get_buffer_slice_from_accessor<'a>(
    accessor: gltf::Accessor<'a>,
    buffers: &'a [gltf::buffer::Data],
) -> &'a [u8] {
    let buffer_view = accessor.view().unwrap();
    let buffer = &buffers[buffer_view.buffer().index()];
    let first_byte_offset = buffer_view.offset() + accessor.offset();
    let last_byte_offset = first_byte_offset + accessor.count() * accessor.size();
    &buffer[first_byte_offset..last_byte_offset]
}

#[profiling::function]
pub fn build_geometry(
    primitive_group: &gltf::mesh::Primitive,
    buffers: &[gltf::buffer::Data],
) -> Result<(BindableGeometryBuffers, BindableIndices)> {
    let vertex_positions = get_vertex_positions(primitive_group, buffers)?;
    let vertex_position_count = vertex_positions.len();
    let bounding_box = crate::collisions::Aabb::make_from_points(vertex_positions.iter().copied())
        .expect("Expected model to have at least two vertex positions");

    let indices = get_indices(primitive_group, buffers, vertex_position_count)?;

    let triangle_count = indices.len() / 3;

    let mut triangles_as_index_tuples = Vec::with_capacity(triangle_count);
    for triangle_index in 0..triangle_count {
        let i_left = triangle_index * 3;
        triangles_as_index_tuples.push((
            indices[i_left] as usize,
            indices[i_left + 1] as usize,
            indices[i_left + 2] as usize,
        ));
    }

    let vertex_tex_coords = get_vertex_tex_coords(primitive_group, buffers, vertex_position_count)?;
    let vertex_tex_coord_count = vertex_tex_coords.len();

    let vertex_colors = get_vertex_colors(primitive_group, buffers, vertex_position_count)?;
    let vertex_color_count = vertex_colors.len();

    let vertex_normals = get_vertex_normals(
        primitive_group,
        buffers,
        &triangles_as_index_tuples,
        &vertex_positions,
        vertex_position_count,
    )?;
    let vertex_normal_count = vertex_normals.len();

    let vertex_bone_indices =
        get_vertex_bone_indices(primitive_group, buffers, vertex_position_count)?;
    let vertex_bone_indices_count = vertex_bone_indices.len();

    let vertex_bone_weights =
        get_vertex_bone_weights(primitive_group, buffers, vertex_position_count)?;
    let vertex_bone_weights_count = vertex_bone_weights.len();

    if vertex_normal_count != vertex_position_count {
        bail!(
          "Expected vertex normals for every vertex but found: vertex_position_count({:?}) != vertex_normal_count({:?})",
          vertex_position_count,
          vertex_normal_count
      );
    }
    if vertex_tex_coord_count != vertex_position_count {
        bail!(
          "Expected vertex normals for every vertex but found: vertex_position_count({:?}) != vertex_tex_coord_count({:?})",
          vertex_position_count,
          vertex_tex_coord_count
      );
    }
    if vertex_color_count != vertex_position_count {
        bail!(
          "Expected vertex colors for every vertex but found: vertex_position_count({:?}) != vertex_color_count({:?})",
          vertex_position_count,
          vertex_color_count
      );
    }
    if vertex_bone_indices_count != vertex_position_count {
        bail!(
          "Expected vertex bone indices for every vertex but found: vertex_position_count({:?}) != vertex_bone_indices_count({:?})",
          vertex_position_count,
          vertex_bone_indices_count
      );
    }
    if vertex_bone_weights_count != vertex_position_count {
        bail!(
          "Expected vertex bone weights for every vertex but found: vertex_position_count({:?}) != vertex_bone_weights_count({:?})",
          vertex_position_count,
          vertex_bone_weights_count
      );
    }

    let vertex_tangents_and_bitangents = get_vertex_tangents(
        primitive_group,
        buffers,
        &vertex_normals,
        triangles_as_index_tuples,
        &vertex_positions,
        &vertex_tex_coords,
    )?;

    let mut vertices = Vec::with_capacity(vertex_position_count);
    for index in 0..vertex_position_count {
        let to_arr = |vec: &Vec3| [vec.x, vec.y, vec.z];
        let (tangent, bitangent) = vertex_tangents_and_bitangents[index];

        vertices.push(Vertex {
            position: vertex_positions[index].into(),
            normal: vertex_normals[index].into(),
            tex_coords: vertex_tex_coords[index],
            tangent: to_arr(&tangent),
            bitangent: to_arr(&bitangent),
            color: vertex_colors[index],
            bone_indices: vertex_bone_indices[index],
            bone_weights: vertex_bone_weights[index],
        });
    }

    let make_bindable_indices = |indices: &Vec<u32>| {
        let mut indices_u16 = Vec::with_capacity(indices.len());
        for index in indices {
            match u16::try_from(*index) {
                Ok(index_u16) => {
                    indices_u16.push(index_u16);
                }
                Err(_) => {
                    // failed to fit a u16 into a u32, so give up and use u32 indices
                    return BindableIndices::U32(indices.to_vec());
                }
            };
        }
        BindableIndices::U16(indices_u16)
    };

    let bindable_indices = make_bindable_indices(&indices);

    let mut wireframe_indices = Vec::with_capacity(indices.len() * 2);
    for triangle in indices.chunks(3) {
        wireframe_indices.extend([
            triangle[0],
            triangle[1],
            triangle[1],
            triangle[2],
            triangle[2],
            triangle[0],
        ]);
    }
    let bindable_wireframe_indices = make_bindable_indices(&wireframe_indices);

    Ok((
        BindableGeometryBuffers {
            vertices,
            indices: bindable_indices,
            bounding_box,
        },
        bindable_wireframe_indices,
    ))
}

fn get_vertex_tangents(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    vertex_normals: &[Vec3],
    triangles_as_index_tuples: Vec<(usize, usize, usize)>,
    vertex_positions: &[Vec3],
    vertex_tex_coords: &[[f32; 2]],
) -> Result<Vec<(Vec3, Vec3)>, anyhow::Error> {
    let vertex_tangents_and_bitangents: Vec<_> = primitive_group
        .attributes()
        .find(|(semantic, _)| *semantic == gltf::Semantic::Tangents)
        .map(|(_, accessor)| {
            let data_type = accessor.data_type();
            let dimensions = accessor.dimensions();
            if dimensions != gltf::accessor::Dimensions::Vec4 {
                bail!("Expected vec3 data but found: {:?}", dimensions);
            }
            if data_type != gltf::accessor::DataType::F32 {
                bail!("Expected f32 data but found: {:?}", data_type);
            }
            let tangents_u8 = get_buffer_slice_from_accessor(accessor, buffers);
            let tangents: &[[f32; 4]] = bytemuck::cast_slice(tangents_u8);

            let mut result = Vec::with_capacity(tangents.len());
            for (vertex_index, tangent_slice) in tangents.iter().enumerate() {
                let normal = vertex_normals[vertex_index];
                let tangent = Vec3::new(tangent_slice[0], tangent_slice[1], tangent_slice[2]);
                // handedness is stored in w component: http://foundationsofgameenginedev.com/FGED2-sample.pdf
                let coordinate_system_handedness = if tangent_slice[3] > 0.0 { -1.0 } else { 1.0 };
                let bitangent = coordinate_system_handedness * normal.cross(tangent);
                result.push((tangent, bitangent));
            }
            Ok(result)
        })
        .transpose()?
        .unwrap_or_else(|| {
            let mut result = Vec::with_capacity(triangles_as_index_tuples.len() * 3);
            for (index_a, index_b, index_c) in triangles_as_index_tuples {
                let make_attributed_point = |index| {
                    (
                        vertex_positions[index],
                        vertex_normals[index],
                        Vec2::from(vertex_tex_coords[index]),
                    )
                };
                let points_with_attribs = [
                    make_attributed_point(index_a),
                    make_attributed_point(index_b),
                    make_attributed_point(index_c),
                ];

                let edge_1 = points_with_attribs[1].0 - points_with_attribs[0].0;
                let edge_2 = points_with_attribs[2].0 - points_with_attribs[0].0;

                let delta_uv_1 = points_with_attribs[1].2 - points_with_attribs[0].2;
                let delta_uv_2 = points_with_attribs[2].2 - points_with_attribs[0].2;

                let f = 1.0 / (delta_uv_1.x * delta_uv_2.y - delta_uv_2.x * delta_uv_1.y);

                let (tangent, bitangent) = {
                    if abs_diff_eq!(f, 0.0, epsilon = 0.00001) || !f.is_finite() {
                        (Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0))
                    } else {
                        let tangent = Vec3::new(
                            f * (delta_uv_2.y * edge_1.x - delta_uv_1.y * edge_2.x),
                            f * (delta_uv_2.y * edge_1.y - delta_uv_1.y * edge_2.y),
                            f * (delta_uv_2.y * edge_1.z - delta_uv_1.y * edge_2.z),
                        );

                        let bitangent = Vec3::new(
                            f * (-delta_uv_2.x * edge_1.x + delta_uv_1.x * edge_2.x),
                            f * (-delta_uv_2.x * edge_1.y + delta_uv_1.x * edge_2.y),
                            f * (-delta_uv_2.x * edge_1.z + delta_uv_1.x * edge_2.z),
                        );

                        (tangent, bitangent)
                    }
                };

                result.extend((0..3).map(|_| (tangent, bitangent)));
            }
            result
        });
    Ok(vertex_tangents_and_bitangents)
}

fn get_vertex_bone_weights(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    vertex_position_count: usize,
) -> Result<Vec<[f32; 4]>, anyhow::Error> {
    let vertex_bone_weights: Vec<[f32; 4]> = primitive_group
        .attributes()
        .find(|(semantic, _)| *semantic == gltf::Semantic::Weights(0))
        .map(|(_, accessor)| {
            let data_type = accessor.data_type();
            let dimensions = accessor.dimensions();
            if dimensions != gltf::accessor::Dimensions::Vec4 {
                bail!("Expected vec4 data but found: {:?}", dimensions);
            }
            if data_type != gltf::accessor::DataType::F32 {
                bail!("Expected f32 data but found: {:?}", data_type);
            }
            let bone_weights_u8 = get_buffer_slice_from_accessor(accessor, buffers);
            Ok(bytemuck::cast_slice::<_, [f32; 4]>(bone_weights_u8).to_vec())
        })
        .transpose()?
        .unwrap_or_else(|| {
            (0..vertex_position_count)
                .map(|_| [1.0, 0.0, 0.0, 0.0])
                .collect()
        });
    Ok(vertex_bone_weights)
}

fn get_vertex_bone_indices(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    vertex_position_count: usize,
) -> Result<Vec<[u32; 4]>, anyhow::Error> {
    let vertex_bone_indices: Vec<[u32; 4]> = primitive_group
        .attributes()
        .find(|(semantic, _)| *semantic == gltf::Semantic::Joints(0))
        .map(|(_, accessor)| {
            let data_type = accessor.data_type();
            let dimensions = accessor.dimensions();
            if dimensions != gltf::accessor::Dimensions::Vec4 {
                bail!("Expected vec4 data but found: {:?}", dimensions);
            }
            let bone_indices_u16 = match data_type {
                gltf::accessor::DataType::U16 => Some(
                    bytemuck::cast_slice::<_, u16>(get_buffer_slice_from_accessor(
                        accessor, buffers,
                    ))
                    .to_vec(),
                ),
                gltf::accessor::DataType::U8 => Some(
                    get_buffer_slice_from_accessor(accessor, buffers)
                        .iter()
                        .map(|res| *res as u16)
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            }
            .ok_or_else(|| anyhow::anyhow!("Expected u8 or u16 data but found: {:?}", data_type))?;
            let bone_indices_u16_grouped = bytemuck::cast_slice::<_, [u16; 4]>(&bone_indices_u16);
            Ok(bone_indices_u16_grouped
                .to_vec()
                .iter()
                .map(|indices| {
                    [
                        indices[0] as u32,
                        indices[1] as u32,
                        indices[2] as u32,
                        indices[3] as u32,
                    ]
                })
                .collect())
        })
        .transpose()?
        .unwrap_or_else(|| (0..vertex_position_count).map(|_| [0, 0, 0, 0]).collect());
    Ok(vertex_bone_indices)
}

fn get_vertex_normals(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    triangles_as_index_tuples: &[(usize, usize, usize)],
    vertex_positions: &[Vec3],
    vertex_position_count: usize,
) -> Result<Vec<Vec3>, anyhow::Error> {
    let vertex_normals: Vec<Vec3> = primitive_group
        .attributes()
        .find(|(semantic, _)| *semantic == gltf::Semantic::Normals)
        .map(|(_, accessor)| {
            let data_type = accessor.data_type();
            let dimensions = accessor.dimensions();
            if dimensions != gltf::accessor::Dimensions::Vec3 {
                bail!("Expected vec3 data but found: {:?}", dimensions);
            }
            if data_type != gltf::accessor::DataType::F32 {
                bail!("Expected f32 data but found: {:?}", data_type);
            }
            let normals_u8 = get_buffer_slice_from_accessor(accessor, buffers);
            let normals: &[[f32; 3]] = bytemuck::cast_slice(normals_u8);
            Ok(normals.to_vec().iter().copied().map(Vec3::from).collect())
        })
        .transpose()?
        .unwrap_or_else(|| {
            // compute normals
            // key is flattened vertex position, value is accumulated normal and count
            let mut vertex_normal_accumulators: HashMap<usize, (Vec3, usize)> = HashMap::new();
            triangles_as_index_tuples
                .iter()
                .copied()
                .for_each(|(index_a, index_b, index_c)| {
                    let a = vertex_positions[index_a];
                    let b = vertex_positions[index_b];
                    let c = vertex_positions[index_c];
                    let a_to_b = Vec3::new(b[0], b[1], b[2]) - Vec3::new(a[0], a[1], a[2]);
                    let a_to_c = Vec3::new(c[0], c[1], c[2]) - Vec3::new(a[0], a[1], a[2]);
                    let normal = a_to_b.cross(a_to_c).normalize();
                    [index_a, index_b, index_c].iter().for_each(|vertex_index| {
                        let (accumulated_normal, count) = vertex_normal_accumulators
                            .entry(*vertex_index)
                            .or_insert((Vec3::new(0.0, 0.0, 0.0), 0));
                        *accumulated_normal += normal;
                        *count += 1;
                    });
                });
            (0..vertex_position_count)
                .map(|vertex_index| {
                    let (accumulated_normal, count) =
                        vertex_normal_accumulators.get(&vertex_index).unwrap();
                    *accumulated_normal / (*count as f32)
                })
                .collect()
        });
    Ok(vertex_normals)
}

fn get_vertex_positions(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
) -> Result<Vec<Vec3>, anyhow::Error> {
    let vertex_positions: Vec<Vec3> = {
        let (_, accessor) = primitive_group
            .attributes()
            .find(|(semantic, _)| *semantic == gltf::Semantic::Positions)
            .ok_or_else(|| anyhow::anyhow!("No positions found"))?;
        let data_type = accessor.data_type();
        let dimensions = accessor.dimensions();
        if dimensions != gltf::accessor::Dimensions::Vec3 {
            bail!("Expected vec3 data but found: {:?}", dimensions);
        }
        if data_type != gltf::accessor::DataType::F32 {
            bail!("Expected f32 data but found: {:?}", data_type);
        }
        let positions_u8 = get_buffer_slice_from_accessor(accessor, buffers);
        let positions: &[[f32; 3]] = bytemuck::cast_slice(positions_u8);
        anyhow::Ok(positions.to_vec().iter().copied().map(Vec3::from).collect())
    }?;
    Ok(vertex_positions)
}

fn get_vertex_colors(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    vertex_position_count: usize,
) -> Result<Vec<[f32; 4]>, anyhow::Error> {
    let vertex_colors: Vec<[f32; 4]> = primitive_group
        .attributes()
        .find(|(semantic, _)| *semantic == gltf::Semantic::Colors(0))
        .map(|(_, accessor)| {
            let data_type = accessor.data_type();
            let dimensions = accessor.dimensions();
            if dimensions != gltf::accessor::Dimensions::Vec4 {
                bail!("Expected vec4 data but found: {:?}", dimensions);
            }
            let buffer_slice = match data_type {
                gltf::accessor::DataType::F32 => {
                    Some(get_buffer_slice_from_accessor(accessor, buffers))
                }
                gltf::accessor::DataType::U8 => {
                    let buffer_slice = get_buffer_slice_from_accessor(accessor, buffers);
                    let mut result: Vec<f32> = Vec::with_capacity(buffer_slice.len());
                    for num in buffer_slice {
                        result.push(*num as f32 / 255.0);
                    }
                    Some(bytemuck::cast_slice::<_, u8>(buffer_slice))
                }
                gltf::accessor::DataType::U16 => {
                    let buffer_slice = bytemuck::cast_slice::<_, u16>(
                        get_buffer_slice_from_accessor(accessor, buffers),
                    );
                    let mut result: Vec<f32> = Vec::with_capacity(buffer_slice.len());
                    for num in buffer_slice {
                        result.push(*num as f32 / 255.0);
                    }
                    Some(bytemuck::cast_slice::<_, u8>(buffer_slice))
                }
                _ => None,
            }
            .ok_or_else(|| {
                anyhow::anyhow!("Expected f32, u8, or u16 data but found: {:?}", data_type)
            })?;
            Ok(bytemuck::cast_slice(buffer_slice).to_vec())
        })
        .transpose()?
        .unwrap_or_else(|| {
            let mut result = Vec::with_capacity(vertex_position_count);
            result.extend((0..vertex_position_count).map(|_| [1.0, 1.0, 1.0, 1.0]));
            result
        });
    Ok(vertex_colors)
}

fn get_vertex_tex_coords(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    vertex_position_count: usize,
) -> Result<Vec<[f32; 2]>, anyhow::Error> {
    let vertex_tex_coords: Vec<[f32; 2]> = primitive_group
        .attributes()
        .find(|(semantic, _)| *semantic == gltf::Semantic::TexCoords(0))
        .map(|(_, accessor)| {
            let data_type = accessor.data_type();
            let dimensions = accessor.dimensions();
            if dimensions != gltf::accessor::Dimensions::Vec2 {
                bail!("Expected vec2 data but found: {:?}", dimensions);
            }
            if data_type != gltf::accessor::DataType::F32 {
                bail!("Expected f32 data but found: {:?}", data_type);
            }
            Ok(bytemuck::cast_slice(get_buffer_slice_from_accessor(accessor, buffers)).to_vec())
        })
        .transpose()?
        .unwrap_or_else(|| (0..vertex_position_count).map(|_| [0.5, 0.5]).collect());
    Ok(vertex_tex_coords)
}

#[profiling::function]
fn get_indices(
    primitive_group: &gltf::Primitive,
    buffers: &[gltf::buffer::Data],
    vertex_position_count: usize,
) -> Result<Vec<u32>, anyhow::Error> {
    let indices: Vec<u32> = primitive_group
        .indices()
        .map(|accessor| {
            let data_type = accessor.data_type();
            let buffer_slice = get_buffer_slice_from_accessor(accessor, buffers);

            let indices: Vec<u32> = match data_type {
                gltf::accessor::DataType::U16 => anyhow::Ok(
                    bytemuck::cast_slice::<_, u16>(buffer_slice)
                        .iter()
                        .map(|&x| x as u32)
                        .collect::<Vec<_>>(),
                ),
                gltf::accessor::DataType::U8 => {
                    anyhow::Ok(buffer_slice.iter().map(|&x| x as u32).collect::<Vec<_>>())
                }
                gltf::accessor::DataType::U32 => {
                    anyhow::Ok(bytemuck::cast_slice::<_, u32>(buffer_slice).to_vec())
                }
                data_type => {
                    bail!("Expected u32, u16 or u8 indices but found: {:?}", data_type)
                }
            }?;
            anyhow::Ok(indices)
        })
        .unwrap_or_else(|| {
            let vertex_position_count_u32 = u32::try_from(vertex_position_count)?;
            Ok((0..vertex_position_count_u32).collect())
        })?;
    Ok(indices)
}
