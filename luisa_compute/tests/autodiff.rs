use std::{env::current_exe, ops::Range};

use luisa::prelude::*;
use luisa::*;
use luisa_compute as luisa;
use rand::prelude::*;
use rayon::{
    prelude::{IntoParallelIterator, ParallelIterator},
    slice::ParallelSliceMut,
};
fn _signal_handler(signal: libc::c_int) {
    if signal == libc::SIGSEGV {
        panic!("segfault detected");
    }
}
static ONCE: std::sync::Once = std::sync::Once::new();
fn get_device() -> Device {
    let show_log = match std::env::var("LUISA_TEST_LOG") {
        Ok(log) => log == "1",
        Err(_) => false,
    };
    ONCE.call_once(|| {
        if show_log {
            init_logger_verbose();
        }
        unsafe {
            libc::signal(libc::SIGSEGV, _signal_handler as usize);
        }
    });
    let curr_exe = current_exe().unwrap();
    let runtime_dir = curr_exe.parent().unwrap().parent().unwrap();
    let ctx = Context::new(runtime_dir);
    let device = match std::env::var("LUISA_TEST_DEVICE") {
        Ok(device) => device,
        Err(_) => "cpu".to_string(),
    };
    ctx.create_device(&device)
}
fn finite_difference(inputs: &[Float], f: impl Fn(&[Float]) -> Float) -> Vec<Float> {
    let eps = 1e-4;

    let mut outputs = vec![];
    for i in 0..inputs.len() {
        let mut inputs_add = inputs.to_vec();
        inputs_add[i] += eps;
        let mut inputs_sub = inputs.to_vec();
        inputs_sub[i] -= eps;
        outputs.push((f(&inputs_add) - f(&inputs_sub)) / Float::from(2.0 * eps));
    }
    outputs
}

fn autodiff_helper<F: Fn(&[Float]) -> Float>(
    range: Range<f32>,
    repeats: usize,
    n_inputs: usize,
    f: F,
) {
    let device = get_device();
    let inputs = (0..n_inputs)
        .map(|_| device.create_buffer::<f32>(repeats))
        .collect::<Vec<_>>();
    let grad_fd = (0..n_inputs)
        .map(|_| device.create_buffer::<f32>(repeats))
        .collect::<Vec<_>>();
    let grad_ad = (0..n_inputs)
        .map(|_| device.create_buffer::<f32>(repeats))
        .collect::<Vec<_>>();
    let tic = std::time::Instant::now();
    let tmp: Vec<Vec<f32>> = (0..n_inputs)
        .into_par_iter()
        .map(|_| {
            let mut rng = rand::thread_rng();
            let mut tmp: Vec<f32> = vec![];
            for _ in 0..repeats {
                tmp.push(rng.gen_range(range.clone()));
            }
            tmp
        })
        .collect::<Vec<_>>();
    for i in 0..n_inputs {
        inputs[i].view(..).copy_from(&tmp[i]);
    }

    // let mut rng = rand::thread_rng();
    // for i in 0..n_inputs {
    //     let mut tmp: Vec<f32> = vec![];
    //     for _ in 0..repeats {
    //         tmp.push(rng.gen_range(range.clone()));
    //     }
    //     inputs[i].view(..).copy_from(&tmp);
    // }
    println!("init time: {:?}", tic.elapsed());
    let kernel = device.create_kernel_async::<()>(&|| {
        let input_vars = inputs.iter().map(|input| input.var()).collect::<Vec<_>>();
        let grad_fd_vars = grad_fd.iter().map(|grad| grad.var()).collect::<Vec<_>>();
        let grad_ad_vars = grad_ad.iter().map(|grad| grad.var()).collect::<Vec<_>>();
        let tid = dispatch_id().x();
        let inputs = input_vars
            .iter()
            .map(|input| input.read(tid))
            .collect::<Vec<_>>();
        autodiff(|| {
            for input in &inputs {
                requires_grad(*input);
            }
            let output = f(&inputs);
            backward(output);
            for i in 0..n_inputs {
                grad_ad_vars[i].write(tid, gradient(inputs[i]));
            }
        });
        let fd = finite_difference(&inputs, &f);
        for i in 0..n_inputs {
            grad_fd_vars[i].write(tid, fd[i]);
        }
    });
    let tic = std::time::Instant::now();
    kernel.dispatch([repeats as u32, 1, 1]);
    println!("kernel time: {:?}", tic.elapsed());
    let grad_ad_datas = grad_ad
        .iter()
        .map(|grad| {
            let mut data = vec![0.0; repeats];
            grad.view(..).copy_to(&mut data);
            data
        })
        .collect::<Vec<_>>();
    let grad_fd_datas = grad_fd
        .iter()
        .map(|grad| {
            let mut data = vec![0.0; repeats];
            grad.view(..).copy_to(&mut data);
            data
        })
        .collect::<Vec<_>>();
    let input_datas = inputs
        .iter()
        .map(|input| {
            let mut data = vec![0.0; repeats];
            input.view(..).copy_to(&mut data);
            data
        })
        .collect::<Vec<_>>();
    let kernel_dir = kernel.cache_dir();
    let mut rel_errors = vec![];
    let mut abs_errors = vec![];
    for r in 0..repeats {
        for i in 0..n_inputs {
            let rel_error = (grad_ad_datas[i][r] - grad_fd_datas[i][r]).abs()
                / (grad_ad_datas[i][r].abs() + 1e-6);
            let abs_error = (grad_ad_datas[i][r] - grad_fd_datas[i][r]).abs();
            assert!(
                abs_error < 5e-2 || rel_error < 5e-2,
                "inputs:{:?} fd: {}, ad: {}, kernel: {:?}",
                (0..n_inputs)
                    .map(|i| input_datas[i][r])
                    .collect::<Vec<f32>>(),
                grad_fd_datas[i][r],
                grad_ad_datas[i][r],
                kernel_dir,
            );
            rel_errors.push(rel_error);
            abs_errors.push(abs_error);
        }
    }
    rel_errors.par_sort_by(|a, b| a.partial_cmp(b).unwrap());
    abs_errors.par_sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_rel_error = rel_errors.iter().sum::<f32>() / rel_errors.len() as f32;
    let mean_abs_error = abs_errors.iter().sum::<f32>() / abs_errors.len() as f32;
    let ninety_ninth_rel_error = rel_errors[(rel_errors.len() * 99) / 100];
    let ninety_ninth_abs_error = abs_errors[(abs_errors.len() * 99) / 100];
    let max_rel_error = *rel_errors
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap();
    let max_abs_error = *abs_errors
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap();
    assert!(
        ninety_ninth_abs_error <= max_abs_error,
        "{} {}",
        ninety_ninth_abs_error,
        max_abs_error
    );
    assert!(
        ninety_ninth_rel_error <= max_rel_error,
        "{} {}",
        ninety_ninth_rel_error,
        max_rel_error
    );
    assert!(
        mean_rel_error < 3e-3 || mean_abs_error < 1e-2 && ninety_ninth_abs_error < 5e-2,
        "mean rel: {}, mean abs: {}, max rel:{} 99% max rel:{}, max abs:{}, 99% max abs:{}, kernel: {:?}",
        mean_rel_error,
        mean_abs_error,
        max_rel_error, max_abs_error,
        ninety_ninth_rel_error,
        ninety_ninth_abs_error,
        kernel_dir,
    );
}
macro_rules! autodiff_1 {
    ($name:ident, $range:expr, $e:expr) => {
        #[test]
        fn $name() {
            autodiff_helper($range, 1024 * 1024, 1, |inputs| {
                let x = inputs[0];
                ($e)(x)
            });
        }
    };
}
macro_rules! autodiff_2 {
    ($name:ident, $range:expr, $e:expr) => {
        #[test]
        fn $name() {
            autodiff_helper($range, 1024 * 1024, 2, |inputs| {
                let x = inputs[0];
                let y = inputs[1];
                ($e)(x, y)
            });
        }
    };
}
macro_rules! autodiff_3 {
    ($name:ident, $range:expr, $e:expr) => {
        #[test]
        fn $name() {
            autodiff_helper($range, 1024 * 1024, 3, |inputs| {
                let x = inputs[0];
                let y = inputs[1];
                let z = inputs[2];
                ($e)(x, y, z)
            });
        }
    };
}
#[derive(Clone, Copy, Debug, Value)]
#[repr(C)]
struct Foo {
    x: f32,
    y: f32,
}

autodiff_2!(autodiff_const, 1.0..10.0, |x: Float, y: Float| {
    let k = 2.0 / const_::<f32>(3.0);
    x * k + y * k
});
autodiff_2!(autodiff_struct, 1.0..10.0, |x: Float, y: Float| {
    let foo = FooExpr::new(x, y);
    let foo = foo.set_x(1.0 + foo.x());
    foo.x() + foo.y()
});
autodiff_1!(autodiff_sin, -10.0..10.0, |x: Float| x.sin());
autodiff_1!(autodiff_cos, -10.0..10.0, |x: Float| x.cos());
autodiff_1!(autodiff_sincos, -10.0..10.0, |x: Float| x.cos() * x.sin());
autodiff_1!(autodiff_sqrt, 0.1..10.0, |x: Float| x.sqrt());
autodiff_1!(autodiff_rsqrt, 0.1..10.0, |x: Float| x.rsqrt());
autodiff_1!(autodiff_exp, -10.0..3.0, |x: Float| x.exp());
autodiff_1!(autodiff_exp2, -10.0..3.0, |x: Float| x.exp2());
autodiff_1!(autodiff_ln, 0.1..10.0, |x: Float| x.ln());
autodiff_1!(autodiff_log2, 0.1..10.0, |x: Float| x.log2());
autodiff_1!(autodiff_log10, 0.1..10.0, |x: Float| x.log10());
autodiff_1!(autodiff_abs, 0.1..10.0, |x: Float| x.abs());
autodiff_1!(autodiff_abs2, -10.0..-0.1, |x: Float| x.abs());
autodiff_1!(autodiff_asin, -0.9..0.9, |x: Float| x.asin());
autodiff_1!(autodiff_acos, -0.9..0.9, |x: Float| x.acos());
autodiff_1!(autodiff_atan, -10.0..10.0, |x: Float| x.atan());
autodiff_1!(autodiff_sinh, -10.0..10.0, |x: Float| x.sinh());
autodiff_1!(autodiff_cosh, -10.0..10.0, |x: Float| x.cosh());
autodiff_1!(autodiff_tanh, -10.0..10.0, |x: Float| x.tanh());

autodiff_2!(autodiff_div, 1.0..10.0, |x: Float, y: Float| x / y);

autodiff_2!(autodiff_pow, 1.0..10.0, |x: Float, y: Float| x.powf(y));
autodiff_3!(autodiff_lerp, 0.0..1.0, |x: Float, y: Float, z: Float| x
    .lerp(y, z));

#[test]
fn autodiff_vec3_reduce_add_manual() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.x() + v.y() + v.z()
    });
}

#[test]
fn autodiff_vec3_reduce_prod_manual() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.x() * v.y() * v.z()
    });
}
#[test]
fn autodiff_vec3_reduce_add() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.reduce_sum()
    });
}
#[test]
fn autodiff_vec3_reduce_mul() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.reduce_prod()
    });
}
#[test]
fn autodiff_vec3_dot() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.dot(v)
    });
}
#[test]
fn autodiff_vec3_length() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.length()
    });
}
#[test]
fn autodiff_vec3_length_squared() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.length_squared()
    });
}
#[test]
fn autodiff_vec3_normalize() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.normalize().x()
    });
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.normalize().y()
    });
    autodiff_helper(-10.0..10.0, 1024 * 1024, 3, |inputs| {
        let x = inputs[0];
        let y = inputs[1];
        let z = inputs[2];
        let v = make_float3(x, y, z);
        v.normalize().z()
    });
}

#[test]
fn autodiff_vec3_cross_x() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 6, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = def(make_float3(ax, ay, az));
        let bx = inputs[3];
        let by = inputs[4];
        let bz = inputs[5];
        let b = def(make_float3(bx, by, bz));
        let v = def(a.cross(*b));
        *v.x()
    });
}
#[test]
fn autodiff_vec3_cross_y() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 6, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = def(make_float3(ax, ay, az));
        let bx = inputs[3];
        let by = inputs[4];
        let bz = inputs[5];
        let b = def(make_float3(bx, by, bz));
        let v = def(a.cross(*b));
        *v.x()
    });
}

#[test]
fn autodiff_vec3_cross_z() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 6, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[3];
        let by = inputs[4];
        let bz = inputs[5];
        let b = make_float3(bx, by, bz);
        let v = a.cross(b);
        v.z()
    });
}
#[test]
fn autodiff_vec3_distance() {
    autodiff_helper(-10.0..10.0, 1024 * 1024, 6, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[3];
        let by = inputs[4];
        let bz = inputs[5];
        let b = make_float3(bx, by, bz);
        a.distance(b)
    });
}
#[test]
fn autodiff_vec3_replace() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 4, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let b = inputs[3];
        let c = a.set_y(b);
        a.dot(c)
    });
}
#[test]
fn autodiff_matmul() {
    autodiff_helper(-4.0..4.0, 1024 * 1024, 12, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[0 + 3];
        let by = inputs[1 + 3];
        let bz = inputs[2 + 3];
        let b = make_float3(bx, by, bz);
        let cx = inputs[0 + 6];
        let cy = inputs[1 + 6];
        let cz = inputs[2 + 6];
        let c = make_float3(cx, cy, cz);
        let dx = inputs[0 + 9];
        let dy = inputs[1 + 9];
        let dz = inputs[2 + 9];
        let d = make_float3(dx, dy, dz);
        let m = Mat3Expr::new(a, b, c);
        let o = m * d;
        o.x()
    });
}
#[test]
fn autodiff_matmul_transpose() {
    autodiff_helper(-4.0..4.0, 1024 * 1024, 12, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[0 + 3];
        let by = inputs[1 + 3];
        let bz = inputs[2 + 3];
        let b = make_float3(bx, by, bz);
        let cx = inputs[0 + 6];
        let cy = inputs[1 + 6];
        let cz = inputs[2 + 6];
        let c = make_float3(cx, cy, cz);
        let dx = inputs[0 + 9];
        let dy = inputs[1 + 9];
        let dz = inputs[2 + 9];
        let d = make_float3(dx, dy, dz);
        let m = Mat3Expr::new(a, b, c);
        let o = m.transpose() * d;
        o.y()
    });
}
#[test]
fn autodiff_matmul_2() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 12, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[0 + 3];
        let by = inputs[1 + 3];
        let bz = inputs[2 + 3];
        let b = make_float3(bx, by, bz);
        let cx = inputs[0 + 6];
        let cy = inputs[1 + 6];
        let cz = inputs[2 + 6];
        let c = make_float3(cx, cy, cz);
        let dx = inputs[0 + 9];
        let dy = inputs[1 + 9];
        let dz = inputs[2 + 9];
        let d = make_float3(dx, dy, dz);
        let m = Mat3Expr::new(a, b, c);
        let o = m * m * d;
        o.z()
    });
}
#[test]
fn autodiff_matmul_4() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 12, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[0 + 3];
        let by = inputs[1 + 3];
        let bz = inputs[2 + 3];
        let b = make_float3(bx, by, bz);
        let cx = inputs[0 + 6];
        let cy = inputs[1 + 6];
        let cz = inputs[2 + 6];
        let c = make_float3(cx, cy, cz);
        let dx = inputs[0 + 9];
        let dy = inputs[1 + 9];
        let dz = inputs[2 + 9];
        let d = make_float3(dx, dy, dz);
        let m = Mat3Expr::new(a, b, c);
        let o = (m * m) * d;
        o.z()
    });
}
#[test]
fn autodiff_matmul_5() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 12, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[0 + 3];
        let by = inputs[1 + 3];
        let bz = inputs[2 + 3];
        let b = make_float3(bx, by, bz);
        let cx = inputs[0 + 6];
        let cy = inputs[1 + 6];
        let cz = inputs[2 + 6];
        let c = make_float3(cx, cy, cz);
        let dx = inputs[0 + 9];
        let dy = inputs[1 + 9];
        let dz = inputs[2 + 9];
        let d = make_float3(dx, dy, dz);
        let m = Mat3Expr::new(a, b, c);
        let o = m.comp_mul(m) * d;
        o.z()
    });
}
#[test]
fn autodiff_mat_det() {
    autodiff_helper(-2.0..2.0, 1024 * 1024, 9, |inputs| {
        let ax = inputs[0];
        let ay = inputs[1];
        let az = inputs[2];
        let a = make_float3(ax, ay, az);
        let bx = inputs[0 + 3];
        let by = inputs[1 + 3];
        let bz = inputs[2 + 3];
        let b = make_float3(bx, by, bz);
        let cx = inputs[0 + 6];
        let cy = inputs[1 + 6];
        let cz = inputs[2 + 6];
        let c = make_float3(cx, cy, cz);
        let m = Mat3Expr::new(a, b, c);
        m.determinant()
    });
}
// #[test]
// fn autodiff_vec3_reduce_min(){
//
//     autodiff_helper(0.1..1.0, 1024 * 1024, 3, |inputs| {
//         let x = inputs[0];
//         let y = inputs[1];
//         let z = inputs[2];
//         let v = make_float3(x, y, z);
//         v.reduce_min()
//     });
// }

// #[test]
// fn autodiff_vec3_reduce_max(){
//
//     autodiff_helper(0.1..1.0, 1024 * 1024, 3, |inputs| {
//         let x = inputs[0];
//         let y = inputs[1];
//         let z = inputs[2];
//         let v = make_float3(x, y, z);
//         v.reduce_max()
//     });
// }
#[test]
fn autodiff_select() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<fn()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let z = select(x.cmpgt(y), x * 4.0, y * 0.5);
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        if x[i] > y[i] {
            assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
        } else {
            assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
        }
    }
}

#[test]
fn autodiff_detach() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<fn()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let k = detach(x * y);
            let z = (x + y) * k;
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        let k = x[i] * y[i];
        assert!(
            (dx[i] - k).abs() < 1e-3,
            "{} cache_dir: {:?}",
            dx[i],
            cache_dir
        );
        assert!(
            (dy[i] - k).abs() < 1e-3,
            "{} cache_dir: {:?}",
            dy[i],
            cache_dir
        );
    }
}
#[test]
fn autodiff_select_nan() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen::<f32>() + 10.0);
    let kernel = device.create_kernel::<fn()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let cond = x.cmpgt(y);
            let a = (x - y).sqrt();
            let z = select(cond, a, y * 0.5);
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        assert!(x[i] < y[i]);
        assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
        assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
    }
}
#[test]
fn autodiff_if_nan() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen::<f32>() + 10.0);
    let kernel = device.create_kernel::<()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let cond = x.cmpgt(y);
            let z = if_!(cond, {
                let a = (x - y).sqrt();
                a
            }, else {
                y * 0.5
            });
            // cpu_dbg!(f32, z);
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        assert!(x[i] < y[i]);
        // if x[i] > y[i] {
        //     assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
        //     assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
        // } else {
        assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
        assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
        // }
    }
}
#[test]
fn autodiff_if_phi() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        if_!(true, {
            autodiff(|| {
                requires_grad(x);
                requires_grad(y);
                let z = if_!(x.cmpgt(y), {
                    x * 4.0
                }, else {
                    y * 0.5
                });
                backward(z);
                buf_dx.write(tid, gradient(x));
                buf_dy.write(tid, gradient(y));
            });
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        if x[i] > y[i] {
            assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
        } else {
            assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
        }
    }
}

#[test]
fn autodiff_if_phi2() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let z = if_!(x.cmpgt(y), {
                if_!(x.cmpgt(3.0), {
                    x * 4.0
                }, else {
                    x * 2.0
                })
            }, else {
                y * 0.5
            });
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        if x[i] > y[i] {
            if x[i] > 3.0 {
                assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            } else {
                assert_eq!(dx[i], 2.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
        } else {
            assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
        }
    }
}
#[test]
fn autodiff_if_phi3() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        let const_two = var!(f32, 2.0);
        let const_three = var!(f32, 3.0);
        let const_four = var!(f32);

        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            const_four.store(4.0);
            let c = x.cmpgt(*const_three).int();
            let z = if_!(x.cmpgt(y), {
                switch::<Expr<f32>>(c)
                    .case(0, || x * *const_two)
                    .default(|| x * *const_four)
                    .finish() * *const_two
            }, else {
                y * 0.5
            });
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        if x[i] > y[i] {
            if x[i] > 3.0 {
                assert_eq!(dx[i], 8.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            } else {
                assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
        } else {
            assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
        }
    }
}
#[test]
fn autodiff_if_phi4() {
    let device = get_device();
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<()>(&|| {
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);

        let consts = var!(Float3);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            consts.store(make_float3(2.0, 3.0, 4.0));
            let const_two = consts.x();
            let const_three = consts.y();
            let const_four = consts.z();
            let c = x.cmpgt(*const_three).int();
            let z = if_!(x.cmpgt(y), {
                switch::<Expr<f32>>(c)
                    .case(0, || x * *const_two)
                    .default(|| x * *const_four)
                    .finish() * *const_two
            }, else {
                y * 0.5
            });
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let x = x.view(..).copy_to_vec();
    let y = y.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        if x[i] > y[i] {
            if x[i] > 3.0 {
                assert_eq!(dx[i], 8.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            } else {
                assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
        } else {
            assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
            assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
        }
    }
}
#[test]
fn autodiff_switch() {
    let device = get_device();
    let t: Buffer<i32> = device.create_buffer(1024);
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    t.view(..).fill_fn(|_| rng.gen_range(0..3));
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let kernel = device.create_kernel::<()>(&|| {
        let buf_t = t.var();
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        let t = buf_t.read(tid);
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let z = switch::<Expr<f32>>(t)
                .case(0, || x * 4.0)
                .case(1, || x * 2.0)
                .case(2, || y * 0.5)
                .finish();
            backward(z);
            buf_dx.write(tid, gradient(x));
            buf_dy.write(tid, gradient(y));
        });
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let t = t.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        match t[i] {
            0 => {
                assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
            1 => {
                assert_eq!(dx[i], 2.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
            2 => {
                assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn autodiff_callable() {
    let device = get_device();
    let t: Buffer<i32> = device.create_buffer(1024);
    let x: Buffer<f32> = device.create_buffer(1024);
    let y: Buffer<f32> = device.create_buffer(1024);
    let dx: Buffer<f32> = device.create_buffer(1024);
    let dy: Buffer<f32> = device.create_buffer(1024);
    let mut rng = rand::thread_rng();
    t.view(..).fill_fn(|_| rng.gen_range(0..3));
    x.view(..).fill_fn(|_| rng.gen());
    y.view(..).fill_fn(|_| rng.gen());
    let callable = device.create_callable::<(Var<f32>, Var<f32>, Expr<i32>), ()>(&|vx, vy, t| {
        let x = *vx;
        let y = *vy;
        autodiff(|| {
            requires_grad(x);
            requires_grad(y);
            let z = switch::<Expr<f32>>(t)
                .case(0, || x * 4.0)
                .case(1, || x * 2.0)
                .case(2, || y * 0.5)
                .finish();
            backward(z);
            *vx.get_mut() = gradient(x);
            *vy.get_mut() = gradient(y);
        });
    });
    let kernel = device.create_kernel::<()>(&|| {
        let buf_t = t.var();
        let buf_x = x.var();
        let buf_y = y.var();
        let buf_dx = dx.var();
        let buf_dy = dy.var();
        let tid = dispatch_id().x();
        let x = buf_x.read(tid);
        let y = buf_y.read(tid);
        let t = buf_t.read(tid);
        let dx = def(x);
        let dy = def(y);
        callable.call(dx, dy, t);
        buf_dx.write(tid, *dx);
        buf_dy.write(tid, *dy);
    });
    kernel.dispatch([1024, 1, 1]);
    let dx = dx.view(..).copy_to_vec();
    let dy = dy.view(..).copy_to_vec();
    let t = t.view(..).copy_to_vec();
    let cache_dir = kernel.cache_dir();
    for i in 0..1024 {
        match t[i] {
            0 => {
                assert_eq!(dx[i], 4.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
            1 => {
                assert_eq!(dx[i], 2.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.0, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
            2 => {
                assert_eq!(dx[i], 0.0, "{} cache_dir: {:?}", dx[i], cache_dir);
                assert_eq!(dy[i], 0.5, "{} cache_dir: {:?}", dy[i], cache_dir);
            }
            _ => unreachable!(),
        }
    }
}
