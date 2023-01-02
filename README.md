# luisa-compute-rs 
Rust binding to LuisaCompute (WIP)

Inside this crate:
- An *almost* safe binding to LuisaCompute
- An EDSL for writing kernels
- A new backend implementation in pure Rust

## Table of Contents
* Example
* Usage
* Safety
## Example (WIP)
```rust
use luisa::prelude::*;
use luisa_compute as luisa;

fn main() {
    init();
    let device = create_cpu_device().unwrap();
    let x = device.create_buffer::<f32>(1024).unwrap();
    let y = device.create_buffer::<f32>(1024).unwrap();
    let z = device.create_buffer::<f32>(1024).unwrap();
    x.view(..).fill_fn(|i| i as f32);
    y.view(..).fill_fn(|i| 1000.0 * i as f32);
    let kernel = device
        .create_kernel(wrap_fn!(1, |buf_z: BufferVar<f32>| {
            // z is pass by arg
            let buf_x = x.var(); // x and y are captured
            let buf_y = y.var();
            let tid = dispatch_id().x();
            let x = buf_x.read(tid);
            let y = buf_y.read(tid);
            buf_z.write(tid, x + y);
        }))
        .unwrap();
    kernel.dispatch([1024, 1, 1], &z).unwrap();
    let mut z_data = vec![0.0; 1024];
    z.view(..).copy_to(&mut z_data);
    println!("{:?}", &z_data[0..16]);
}


```

## Usage (WIP)

## Safety
### API
The API is safe to a large extent. However, async operations are difficult to be completely safe without requiring users to write boilerplate. Thus, all async operations are marked unsafe. 

### Backend 
Safety checks such as OOB is generally not available for GPU backends. As it is difficult to produce meaningful debug message in event of a crash. However, the Rust backend provided in the crate contains full safety checks and is recommended for debugging.
