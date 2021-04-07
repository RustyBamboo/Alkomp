use futures::executor::block_on;
use std::collections::HashMap;
use std::marker::PhantomData;
use wgpu::util::DeviceExt;

#[cfg(not(target_arch = "wasm32"))]
pub fn query() -> Vec<DeviceInfo> {
    let instance = wgpu::Instance::new(wgpu::BackendBit::PRIMARY);
    let adapter = instance.enumerate_adapters(wgpu::BackendBit::PRIMARY);

    let devices: Vec<DeviceInfo> = adapter.map(|a| DeviceInfo { info: a.get_info() }).collect();
    devices
}

pub fn load_shader(path: &str) -> Vec<u32> {
    const MAGIC_NUMBER: u32 = 0x0723_0203;
    let bytes = std::fs::read(path).unwrap();

    let (_, words, _) = unsafe { bytes.align_to::<u32>() };
    assert_eq!(
        words[0], MAGIC_NUMBER,
        "wrong magic word {:x}. Make sure you are using a binary SPIRV file.",
        words[0]
    );
    Vec::from(words)
}

#[cfg(not(target_arch = "wasm32"))]
pub struct Device {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: Option<DeviceInfo>,
}
#[cfg(target_arch = "wasm32")]
pub struct Device {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

pub struct GPUData<T: ?Sized> {
    pub staging_buffer: wgpu::Buffer,
    pub storage_buffer: wgpu::Buffer,
    pub size: u64,
    pub phantom: PhantomData<T>,
}

impl Device {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(device_index: usize) -> Self {
        let instance = wgpu::Instance::new(wgpu::BackendBit::PRIMARY);
        let mut adapter = instance.enumerate_adapters(wgpu::BackendBit::PRIMARY);
        let adapter = adapter.nth(device_index).unwrap();
        let (device, queue) = block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                features: wgpu::Features::MAPPABLE_PRIMARY_BUFFERS,
                limits: wgpu::Limits::default(),
                label: None,
            },
            None,
        ))
        .unwrap();
        let info = adapter.get_info().clone();
        let info = DeviceInfo { info };

        Device {
            device,
            queue,
            info: Some(info),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub async fn new() -> Self {
        let instance = wgpu::Instance::new(wgpu::BackendBit::PRIMARY);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    // features: wgpu::Features::MAPPABLE_PRIMARY_BUFFERS,
                    features: wgpu::Features::empty(),
                    limits: wgpu::Limits::default(),
                    shader_validation: false,
                },
                None,
            )
            .await
            .unwrap();

        Device { device, queue }
    }

    pub fn to_device<T: bytemuck::Pod>(&mut self, data: &[T]) -> GPUData<[T]> {
        let bytes = bytemuck::cast_slice(data);

        // On native we can share memory between CPU and GPU... but not in web
        let staging_buffer = if cfg!(not(target_arch = "wasm32")) {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("Staging Buffer"),
                    contents: &bytes,
                    usage: wgpu::BufferUsage::MAP_READ
                        | wgpu::BufferUsage::COPY_DST
                        | wgpu::BufferUsage::COPY_SRC,
                })
        } else {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bytes.len() as u64,
                usage: wgpu::BufferUsage::MAP_READ | wgpu::BufferUsage::COPY_DST,
                mapped_at_creation: false,
            })
        };

        let storage_buffer = if cfg!(not(target_arch = "wasm32")) {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bytes.len() as u64,
                usage: wgpu::BufferUsage::STORAGE
                    | wgpu::BufferUsage::COPY_DST
                    | wgpu::BufferUsage::COPY_SRC,
                mapped_at_creation: false,
            })
        } else {
            // Preload the buffer with data if we are on web
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("Storage Buffer"),
                    contents: &bytes,
                    usage: wgpu::BufferUsage::STORAGE
                        | wgpu::BufferUsage::COPY_DST
                        | wgpu::BufferUsage::COPY_SRC,
                })
        };

        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            encoder.copy_buffer_to_buffer(
                &staging_buffer,
                0,
                &storage_buffer,
                0,
                bytes.len() as u64,
            );

            self.queue.submit(Some(encoder.finish()));
        }

        GPUData {
            staging_buffer,
            storage_buffer,
            size: bytes.len() as u64,
            phantom: PhantomData,
        }
    }

    pub async fn get<T>(&mut self, gpu: &GPUData<[T]>) -> Option<Box<[T]>>
    where
        T: bytemuck::Pod,
    {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_buffer_to_buffer(&gpu.storage_buffer, 0, &gpu.staging_buffer, 0, gpu.size);
        self.queue.submit(Some(encoder.finish()));

        let buffer_slice = gpu.staging_buffer.slice(0..);
        let buffer_future = buffer_slice.map_async(wgpu::MapMode::Read);

        self.device.poll(wgpu::Maintain::Wait);

        // Gets contents of buffer
        if let Ok(()) = buffer_future.await {
            let data = buffer_slice.get_mapped_range();
            let result = data
                .chunks_exact(std::mem::size_of::<T>())
                .map(|b| bytemuck::from_bytes::<T>(b).clone())
                .collect();
            drop(data);
            gpu.staging_buffer.unmap();
            return Some(result);
        }
        None
    }

    pub fn compile(
        &self,
        entry: &str,
        shader: &wgpu::ShaderModuleDescriptor,
        params: &GPUSetGroupLayout,
    ) -> Result<GPUCompute, ()> {
        let mut bind_group_layouts: HashMap<u32, wgpu::BindGroupLayout> = HashMap::new();
        let mut param_types = HashMap::new();

        for (set_id, set) in &params.set_bind_group_layouts {
            for (binding_num, binding) in set {
                if !param_types.contains_key(&set_id) {
                    param_types.insert(set_id, HashMap::new());
                }
                param_types
                    .get_mut(&set_id)
                    .unwrap()
                    .insert(*binding_num, binding.1.clone());
            }
            bind_group_layouts.insert(
                *set_id,
                self.device
                    .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: None,
                        entries: set
                            .values()
                            .map(|binding_layout| binding_layout.0.clone())
                            .collect::<Vec<wgpu::BindGroupLayoutEntry>>()
                            .as_slice(),
                    }),
            );
        }

        let cs_module = self.device.create_shader_module(shader);

        let pipeline_layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: bind_group_layouts
                    .values()
                    .collect::<Vec<&wgpu::BindGroupLayout>>()
                    .as_slice(),
                push_constant_ranges: &[],
            });

        let pipeline = self
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(&pipeline_layout),
                module: &cs_module,
                entry_point: entry,
            });

        Ok(GPUCompute {
            // param_types,
            bind_group_layouts,
            compute_pipeline: pipeline,
        })
    }

    pub fn call<'a>(
        &mut self,
        gpu_compute: GPUCompute,
        workspace: (u32, u32, u32),
        args: &HashMap<u32, wgpu::BindGroupEntry<'a>>,
    ) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        let set_num = 0;
        let mut bind_groups = vec![];
        // for (set_num, bind_group) in &args {
        bind_groups.push(
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, // TODO maybe in all these label fields, we should actually use a label
                layout: &gpu_compute.bind_group_layouts[&set_num],
                entries: args
                    .values()
                    .map(|binding| binding.clone())
                    .collect::<Vec<wgpu::BindGroupEntry>>()
                    .as_slice(),
            }),
        );
        // }
        {
            let mut cpass =
                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
            cpass.set_pipeline(&gpu_compute.compute_pipeline);

            for (set_num, _bind_group) in gpu_compute.bind_group_layouts {
                // bind_group = collection of bindings
                // let offsets : Vec<u32>= (0..args.len()-1).map(|_| 0).collect();
                cpass.set_bind_group(set_num, &bind_groups[set_num as usize], &[]);
            }
            cpass.dispatch(workspace.0, workspace.1, workspace.2);
        }
        self.queue.submit(Some(encoder.finish()));
    }
}

pub struct GPUCompute {
    // param_types: HashMap<u32, HashMap<u32, String>>,
    bind_group_layouts: HashMap<u32, wgpu::BindGroupLayout>,
    compute_pipeline: wgpu::ComputePipeline,
}

pub struct GPUSetGroupLayout {
    pub set_bind_group_layouts: HashMap<u32, HashMap<u32, (wgpu::BindGroupLayoutEntry, String)>>,
}

///
/// Helper to create the layout of bindings (along with set information.)
/// This returns a `GPUSetGroupLayout` which is a HashMap with a key for a set,
/// which contains a HashMap of Layout index and BindGroupLayoutEntry
/// ```
///     let args = alkomp::ParamsBuilder::new()
///         .param::<&[i32]>(None)
///         .param::<f32>(None)
///         .build(Some(0));
/// ```
///
///
pub struct ParamsBuilder<'a> {
    pub binding_layouts: HashMap<u32, (wgpu::BindGroupLayoutEntry, String)>,
    pub binding_entry: HashMap<u32, wgpu::BindGroupEntry<'a>>,
}

impl<'a> ParamsBuilder<'a> {
    pub fn new() -> Self {
        Self {
            binding_layouts: HashMap::new(),
            binding_entry: HashMap::new(),
        }
    }

    pub fn param<T: Sized>(mut self, gpu_data: Option<&'a GPUData<[T]>>) -> Self {
        let new_binding_layout_idx = self.binding_layouts.len() as u32;
        // println!("{}", String::from(core::any::type_name::<T>()));
        // println!("{}",)

        self.binding_layouts.insert(
            new_binding_layout_idx,
            (
                wgpu::BindGroupLayoutEntry {
                    binding: new_binding_layout_idx,
                    visibility: wgpu::ShaderStage::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                String::from(core::any::type_name::<T>()),
            ),
        );

        if let Some(gpu) = gpu_data {
            // let x = Rc::new(gpu.storage_buffer);
            self.binding_entry.insert(
                new_binding_layout_idx,
                wgpu::BindGroupEntry {
                    binding: new_binding_layout_idx,
                    resource: gpu.storage_buffer.as_entire_binding(),
                },
            );
        }
        self
    }

    pub fn build(
        self,
        set: Option<u32>,
    ) -> (GPUSetGroupLayout, HashMap<u32, wgpu::BindGroupEntry<'a>>) {
        let mut set_bind_group_layouts = HashMap::new();
        set_bind_group_layouts.insert(
            match set {
                Some(s) => s,
                None => 0,
            },
            self.binding_layouts,
        );
        (
            GPUSetGroupLayout {
                set_bind_group_layouts,
            },
            self.binding_entry,
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct DeviceInfo {
    pub info: wgpu::AdapterInfo,
}

#[cfg(not(target_arch = "wasm32"))]
impl DeviceInfo {
    pub fn name(&self) -> String {
        self.info.name.clone()
    }
    pub fn device_type(&self) -> wgpu::DeviceType {
        self.info.device_type.clone()
    }
    pub fn backend(&self) -> &str {
        match self.info.backend {
            wgpu::Backend::Vulkan => "Vulkan",
            wgpu::Backend::Metal => "Metal",
            wgpu::Backend::Dx11 => "Dx11",
            wgpu::Backend::Dx12 => "Dx12",
            wgpu::Backend::BrowserWebGpu => "Browse",
            _ => "Unknown",
        }
    }
    pub fn vendor_id(&self) -> usize {
        self.info.vendor
    }
    pub fn device_id(&self) -> usize {
        self.info.device
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ name: {:?}, vendor_id: {:?}, device_id: {:?}, device_type: {:?} }}",
            self.name(),
            self.vendor_id(),
            self.device_id(),
            self.device_type()
        )
    }
}
