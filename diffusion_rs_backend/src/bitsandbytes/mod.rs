use std::sync::Arc;

use diffusion_rs_common::core::{DType, Device, Result, Shape, Tensor};
use diffusion_rs_common::VarBuilder;
use serde::Deserialize;

use crate::{QuantMethod, QuantMethodConfig};

#[cfg(feature = "cuda")]
mod ffi;

mod op;

const SUPPORTED_BLOCKSIZE: [usize; 7] = [2048, 4096, 1024, 512, 256, 128, 64];

#[derive(Debug, Deserialize, Clone, Copy)]
pub enum BnbDType {
    #[serde(rename = "float32")]
    F32,
    #[serde(rename = "bfloat16")]
    BF16,
    #[serde(rename = "float16")]
    F16,
}

#[derive(Debug, Clone, Copy)]
pub enum BnbQuantType {
    Int8,
    Fp4,
    Nf4,
}

impl From<BnbDType> for DType {
    fn from(value: BnbDType) -> Self {
        match value {
            BnbDType::F32 => Self::F32,
            BnbDType::BF16 => Self::BF16,
            BnbDType::F16 => Self::F16,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BnbQuantState {
    pub blocksize: usize,
    pub shape: Vec<usize>,
    pub dtype: BnbDType,
    pub nested_blocksize: Option<usize>,
    pub nested_offset: Option<f64>,
    pub nested_dtype: Option<BnbDType>,
}

#[derive(Debug, Clone)]
pub struct BnbQuantParmas {
    pub absmax: Tensor,
    pub code: Tensor,
    pub blocksize: usize,
    pub shape: Option<Shape>,
    pub nested: Option<Arc<BnbQuantParmas>>,
    pub offset: Option<f64>,
    pub dtype: BnbDType,
}

impl BnbQuantParmas {
    fn to_device(&self, dev: &Device) -> Result<Self> {
        let absmax = self.absmax.to_device(dev)?;
        let code = self.code.to_device(dev)?;
        let nested = if let Some(nested) = &self.nested {
            Some(Arc::new(nested.to_device(dev)?))
        } else {
            None
        };
        Ok(Self {
            absmax,
            code,
            blocksize: self.blocksize,
            shape: self.shape.clone(),
            nested,
            offset: self.offset,
            dtype: self.dtype,
        })
    }

    fn size_in_bytes(&self) -> Result<usize> {
        let absmax = self.absmax.dtype().size_in_bytes() * self.absmax.elem_count();
        let code = self.code.dtype().size_in_bytes() * self.code.elem_count();
        let nested = if let Some(nested) = &self.nested {
            nested.size_in_bytes()?
        } else {
            0
        };
        Ok(absmax + code + nested)
    }
}

#[derive(Debug)]
pub enum BnbLinear {
    Fp4Nf4 {
        weight: Tensor,
        bias: Option<Tensor>,
        params: BnbQuantParmas,
        quant_ty: BnbQuantType,
    },
    Int8 {
        weight: Tensor,
        scb: Tensor,
        bias: Option<Tensor>,
    },
}

impl BnbLinear {
    pub fn linear_b(in_dim: usize, out_dim: usize, bias: bool, vb: VarBuilder) -> Result<Self> {
        if vb.contains_tensor("SCB") {
            Self::linear_8bit(in_dim, out_dim, bias, vb)
        } else if vb.contains_tensor("weight.quant_state.bitsandbytes__nf4")
            || vb.contains_tensor("weight.quant_state.bitsandbytes__fp4")
        {
            Self::linear_4bit(in_dim, out_dim, bias, vb)
        } else {
            diffusion_rs_common::bail!("`BnbLinear` expects fp4/nf4 or int8 layers.");
        }
    }

    fn linear_8bit(_in_dim: usize, out_dim: usize, bias: bool, vb: VarBuilder) -> Result<Self> {
        let weight = vb.get_unchecked_dtype("weight", DType::I8)?;
        let scb = vb.get_unchecked_dtype("SCB", DType::F32)?;

        let bias = if bias {
            Some(vb.get((out_dim,), "bias")?)
        } else {
            None
        };

        Ok(Self::Int8 { weight, scb, bias })
    }

    fn linear_4bit(_in_dim: usize, out_dim: usize, bias: bool, vb: VarBuilder) -> Result<Self> {
        let weight = vb.get_unchecked_dtype("weight", DType::U8)?;

        let vb_w = vb.pp("weight");

        if !vb_w.contains_tensor("quant_state.bitsandbytes__nf4")
            && !vb_w.contains_tensor("quant_state.bitsandbytes__fp4")
        {
            diffusion_rs_common::bail!("`BnbLinear` expects either `...__nf4` or `...__fp4` tensors, this means the layer is not 4bit or 8big.");
        }

        let quant_ty = if vb_w.contains_tensor("quant_state.bitsandbytes__nf4") {
            BnbQuantType::Nf4
        } else if vb_w.contains_tensor("quant_state.bitsandbytes__fp4") {
            BnbQuantType::Fp4
        } else {
            BnbQuantType::Int8
        };

        let state = match quant_ty {
            BnbQuantType::Nf4 => {
                Some(vb_w.get_unchecked_dtype("quant_state.bitsandbytes__nf4", DType::U8)?)
            }
            BnbQuantType::Fp4 => {
                Some(vb_w.get_unchecked_dtype("quant_state.bitsandbytes__fp4", DType::U8)?)
            }
            BnbQuantType::Int8 => None,
        };
        let Some(state) = state else {
            diffusion_rs_common::bail!("Only fp8/nf4 quantization is supported for now.")
        };

        let state_str = String::from_utf8(state.to_vec1::<u8>()?)?;
        let state: BnbQuantState =
            serde_json::from_str(&state_str).map_err(diffusion_rs_common::core::Error::msg)?;

        let nested = if vb_w.contains_tensor("nested_absmax") {
            // TODO: can `nested_blocksize` be None, default to 64 like bnb?
            Some(Arc::new(BnbQuantParmas {
                absmax: vb_w.get_unchecked_dtype("nested_absmax", DType::F32)?,
                code: vb_w.get_unchecked_dtype("nested_quant_map", DType::F32)?,
                blocksize: state.nested_blocksize.ok_or(
                    diffusion_rs_common::core::Error::debug("`nested_blocksize` must be present."),
                )?,
                shape: None,
                nested: None,
                offset: None, // Put it in the outer one!
                dtype: state
                    .nested_dtype
                    .ok_or(diffusion_rs_common::core::Error::debug(
                        "`nested_dtype` must be present.",
                    ))?,
            }))
        } else {
            None
        };

        let absmax = if nested.is_some() {
            vb_w.get_unchecked_dtype("absmax", DType::U8)?
        } else {
            vb_w.get_unchecked_dtype("absmax", DType::F32)?
        };

        let params = BnbQuantParmas {
            absmax,
            code: vb_w.get_unchecked_dtype("quant_map", DType::F32)?,
            blocksize: state.blocksize,
            shape: Some(Shape::from_dims(&state.shape)),
            nested,
            offset: state.nested_offset,
            dtype: state.dtype,
        };

        let bias = if bias {
            Some(vb.get((out_dim,), "bias")?.to_dtype(params.dtype.into())?)
        } else {
            None
        };

        Ok(Self::Fp4Nf4 {
            weight,
            bias,
            params,
            quant_ty,
        })
    }

    /// Dequantize input (u8). Handles nested absmax dequantization.
    fn dequantize_4bit(
        input: &Tensor,
        params: &BnbQuantParmas,
        quant_ty: BnbQuantType,
    ) -> Result<Tensor> {
        let mut absmax = params.absmax.clone();
        if let Some(nested) = &params.nested {
            absmax = Self::dequantize_4bit(&params.absmax, nested, BnbQuantType::Int8)?;
            absmax = (absmax
                + params
                    .offset
                    .ok_or(diffusion_rs_common::core::Error::debug(
                        "`offset` must be present.",
                    ))?)?;
        }

        let out_shape = params.shape.clone().unwrap_or(input.shape().clone());
        let out_dtype: DType = params.dtype.into();

        if !SUPPORTED_BLOCKSIZE.contains(&params.blocksize) {
            diffusion_rs_common::bail!(
                "Blocksize of {} is not supported, {SUPPORTED_BLOCKSIZE:?} are.",
                params.blocksize
            );
        }

        op::dequantize(
            input,
            &absmax,
            &params.code,
            out_shape,
            params.blocksize,
            quant_ty,
            params.dtype,
        )?
        .to_dtype(out_dtype)
    }
}

impl QuantMethod for BnbLinear {
    fn new(method: QuantMethodConfig) -> diffusion_rs_common::core::Result<Self>
    where
        Self: Sized,
    {
        match method {
            QuantMethodConfig::Gguf { .. } | QuantMethodConfig::Unquantized(_) => unreachable!(),
            QuantMethodConfig::Bnb4bit {
                weight,
                bias,
                params,
                quant_ty,
            } => Ok(Self::Fp4Nf4 {
                weight,
                bias,
                params,
                quant_ty,
            }),
        }
    }

    fn dequantize_w(&self, out_ty: DType) -> Result<Tensor> {
        match self {
            Self::Fp4Nf4 {
                weight,
                bias: _,
                params,
                quant_ty,
            } => Self::dequantize_4bit(weight, params, *quant_ty),
            Self::Int8 {
                weight,
                scb,
                bias: _,
            } => op::dequantize_8bit(weight, scb, out_ty),
        }
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let w = self.dequantize_w(xs.dtype())?.t()?;
        let res = xs.broadcast_matmul(&w)?;
        let bias = match self {
            Self::Fp4Nf4 { bias, .. } | Self::Int8 { bias, .. } => bias,
        };
        if let Some(bias) = bias {
            res.broadcast_add(&bias.to_dtype(res.dtype())?)
        } else {
            Ok(res)
        }
    }

    fn quantized_act_type(&self) -> Option<DType> {
        None
    }

    fn to_device(&self, dev: &Device) -> Result<Arc<dyn QuantMethod>> {
        match self {
            Self::Fp4Nf4 {
                weight,
                bias,
                params,
                quant_ty,
            } => {
                let weight = weight.to_device(dev)?;
                let bias = if let Some(bias) = bias {
                    Some(bias.to_device(dev)?)
                } else {
                    None
                };
                let params = params.to_device(dev)?;
                Ok(Arc::new(Self::Fp4Nf4 {
                    weight,
                    bias,
                    params,
                    quant_ty: *quant_ty,
                }))
            }
            Self::Int8 { weight, scb, bias } => {
                let weight = weight.to_device(dev)?;
                let scb = scb.to_device(dev)?;
                let bias = if let Some(bias) = bias {
                    Some(bias.to_device(dev)?)
                } else {
                    None
                };
                Ok(Arc::new(Self::Int8 { weight, scb, bias }))
            }
        }
    }

    fn size_in_bytes(&self) -> Result<usize> {
        match self {
            Self::Fp4Nf4 {
                weight,
                bias,
                params,
                quant_ty: _,
            } => {
                let w_size = weight.dtype().size_in_bytes() * weight.elem_count();
                let params_size = params.size_in_bytes()?;
                let b_size = if let Some(b) = bias {
                    b.dtype().size_in_bytes() * b.elem_count()
                } else {
                    0
                };
                Ok(w_size + params_size + b_size)
            }
            Self::Int8 { weight, scb, bias } => {
                let w_size = weight.dtype().size_in_bytes() * weight.elem_count();
                let scb_size = scb.dtype().size_in_bytes() * scb.elem_count();
                let b_size = if let Some(b) = bias {
                    b.dtype().size_in_bytes() * b.elem_count()
                } else {
                    0
                };
                Ok(w_size + scb_size + b_size)
            }
        }
    }

    fn device(&self) -> Device {
        match self {
            Self::Fp4Nf4 { weight, .. } | Self::Int8 { weight, .. } => weight.device().clone(),
        }
    }
}
