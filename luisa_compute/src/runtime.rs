use crate::backend::Backend;
use crate::*;
use crate::{lang::Value, resource::*};
pub use luisa_compute_api_types as api;
use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;
use std::{ffi::CString, path::PathBuf};
#[derive(Clone)]
pub struct Device {
    pub(crate) inner: Arc<DeviceHandle>,
}
pub(crate) struct DeviceHandle {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) default_stream: api::Stream,
}
impl Deref for DeviceHandle {
    type Target = dyn Backend;
    fn deref(&self) -> &Self::Target {
        self.backend.deref()
    }
}
unsafe impl Send for Device {}
unsafe impl Sync for Device {}
// pub struct Context {
//     pub(crate) inner: sys::LCContext,
// }
// unsafe impl Send for Context {}
// unsafe impl Sync for Context {}
// impl Context {
//     pub fn new() -> Self {
//         let exe_path = std::env::current_exe().unwrap();
//         catch_abort! {{
//             let exe_path = CString::new(exe_path.to_str().unwrap()).unwrap();
//             let ctx = sys::luisa_compute_context_create(exe_path.as_ptr());
//             Self { inner: ctx }
//         }}
//     }
//     pub fn create_device(&self, device: &str, properties: serde_json::Value) -> Device {
//         catch_abort! {{
//             let device = CString::new(device).unwrap();
//             let properties = CString::new(properties.to_string()).unwrap();
//             let device =
//                 sys::luisa_compute_device_create(self.inner, device.as_ptr(), properties.as_ptr());
//             let default_stream = sys::luisa_compute_stream_create(device);
//             Device {
//                 inner: Arc::new(DeviceHandle{
//                     handle:device,
//                     default_stream
//                 })
//         }
//         }}
//     }
//     pub fn runtime_dir(&self) -> PathBuf {
//         catch_abort! {{
//             let path = sys::luisa_compute_context_runtime_directory(self.inner);
//             let path = std::ffi::CStr::from_ptr(path).to_str().unwrap().to_string();
//             PathBuf::from(path)
//         }}
//     }
//     pub fn cache_dir(&self) -> PathBuf {
//         catch_abort! {{
//             let path = sys::luisa_compute_context_cache_directory(self.inner);
//             let path = std::ffi::CStr::from_ptr(path).to_str().unwrap().to_string();
//             PathBuf::from(path)
//         }}
//     }
// }
// impl Drop for Context {
//     fn drop(&mut self) {
//         catch_abort! {{
//             sys::luisa_compute_context_destroy(self.inner);
//         }}
//     }
// }

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        self.backend.destroy_stream(self.default_stream);
    }
}
impl Device {
    pub fn create_buffer<T: Value>(&self, count: usize) -> backend::Result<Buffer<T>> {
        let buffer = self.inner.create_buffer(std::mem::size_of::<T>() * count)?;
        Ok(Buffer {
            device: self.clone(),
            handle: Arc::new(BufferHandle {
                device: self.clone(),
                handle: buffer,
            }),
            _marker: std::marker::PhantomData {},
            len: count,
        })
    }
    pub fn create_bindless_array(&self, slots: usize) -> backend::Result<BindlessArray> {
        let array = self.inner.create_bindless_array(slots)?;
        Ok(BindlessArray {
            device: self.clone(),
            handle: Arc::new(BindlessArrayHandle {
                device: self.clone(),
                handle: array,
            }),
        })
    }
    pub fn create_tex2d<T: Texel>(
        &self,
        format: PixelFormat,
        width: u32,
        height: u32,
        mips: u32,
    ) -> backend::Result<Tex2D<T>> {
        assert!(T::pixel_formats().contains(&format));

        let texture = self
            .inner
            .create_texture(format, 2, width, height, 1, mips)?;
        let handle = Arc::new(TextureHandle {
            device: self.clone(),
            handle: texture,
            format,
        });
        Ok(Tex2D {
            handle,
            marker: std::marker::PhantomData {},
        })
    }
    pub fn create_tex3d<T: Texel>(
        &self,
        format: PixelFormat,
        width: u32,
        height: u32,
        depth: u32,
        mips: u32,
    ) -> backend::Result<Tex3D<T>> {
        assert!(T::pixel_formats().contains(&format));

        let texture = self
            .inner
            .create_texture(format, 3, width, height, depth, mips)?;
        let handle = Arc::new(TextureHandle {
            device: self.clone(),
            handle: texture,
            format,
        });
        Ok(Tex3D {
            handle,
            marker: std::marker::PhantomData {},
        })
    }
    pub fn default_stream(&self) -> Stream {
        Stream {
            device: self.clone(),
            handle: Arc::new(StreamHandle::Default(
                self.inner.clone(),
                self.inner.default_stream,
            )),
        }
    }
    pub fn create_stream(&self) -> backend::Result<Stream> {
        let stream = self.inner.create_stream()?;
        Ok(Stream {
            device: self.clone(),
            handle: Arc::new(StreamHandle::NonDefault {
                device: self.inner.clone(),
                handle: stream,
            }),
        })
    }
}
pub(crate) enum StreamHandle {
    Default(Arc<DeviceHandle>, api::Stream),
    NonDefault {
        device: Arc<DeviceHandle>,
        handle: api::Stream,
    },
}
pub struct Stream {
    pub(crate) device: Device,
    pub(crate) handle: Arc<StreamHandle>,
}
impl StreamHandle {
    pub(crate) fn device(&self) -> Arc<DeviceHandle> {
        match self {
            StreamHandle::Default(device, _) => device.clone(),
            StreamHandle::NonDefault { device, .. } => device.clone(),
        }
    }
    pub(crate) fn handle(&self) -> api::Stream {
        match self {
            StreamHandle::Default(_, stream) => *stream,
            StreamHandle::NonDefault { handle, .. } => *handle,
        }
    }
}
impl Drop for StreamHandle {
    fn drop(&mut self) {
        match self {
            StreamHandle::Default(_, _) => {}
            StreamHandle::NonDefault { device, handle } => {
                device.destroy_stream(*handle);
            }
        }
    }
}
impl Stream {
    pub fn handle(&self) -> api::Stream {
        self.handle.handle()
    }
    pub fn synchronize(&self) -> backend::Result<()> {
        self.handle.device().synchronize_stream(self.handle())
    }
    pub fn command_buffer<'a>(&self) -> CommandBuffer<'a> {
        CommandBuffer::<'a> {
            marker: std::marker::PhantomData {},
            stream: self.handle.clone(),
            commands: Vec::new(),
        }
    }
}
pub struct CommandBuffer<'a> {
    stream: Arc<StreamHandle>,
    marker: std::marker::PhantomData<&'a ()>,
    commands: Vec<Command<'a>>,
}
impl<'a> CommandBuffer<'a> {
    pub fn extend<I: IntoIterator<Item = Command<'a>>>(&mut self, commands: I) {
        self.commands.extend(commands);
    }
    pub fn push(&mut self, command: Command<'a>) {
        self.commands.push(command);
    }
    pub fn commit(self) -> backend::Result<()> {
        let commands = self.commands.iter().map(|c| c.inner).collect::<Vec<_>>();
        self.stream
            .device()
            .dispatch(self.stream.handle(), &commands)
    }
}

pub fn submit_default_stream_and_sync<'a, I: IntoIterator<Item = Command<'a>>>(
    device: &Device,
    commands: I,
) -> backend::Result<()> {
    let default_stream = device.default_stream();
    let mut cmd_buffer = default_stream.command_buffer();

    cmd_buffer.extend(commands);

    cmd_buffer.commit()?;
    default_stream.synchronize()
}
pub struct Command<'a> {
    #[allow(dead_code)]
    pub(crate) inner: api::Command,
    pub(crate) marker: std::marker::PhantomData<&'a ()>,
    #[allow(dead_code)]
    pub(crate) resource_tracker: Vec<Box<dyn Any>>,
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_layout() {
        assert_eq!(
            std::mem::size_of::<api::Command>(),
            std::mem::size_of::<sys::LCCommand>()
        );
    }
}
