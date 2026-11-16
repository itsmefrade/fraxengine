//! Numeric foundation only: custom RPU YCC -> absolute BT2020 RGB -> PQ/NCL.
//! No RPU parsing, CM trims, tone mapping, implicit chroma upsampling, or display.
#[derive(Clone,Copy,Debug,PartialEq)]pub enum Error{MetadataTuple,OffsetDomain,SingularMatrix,LuminanceBounds,InputDomain,NonFinite,PqDomain,GamutDomain,UnsupportedRange,Quantization}
#[derive(Clone,Copy,Debug,Default)]pub enum PqPolicy{#[default]Reject,ClampDiagnostic}
#[derive(Clone,Copy,Debug,Default)]pub enum GamutPolicy{#[default]Reject,ClipDiagnostic}
#[derive(Clone,Copy,Debug)]pub enum Range{Narrow10,Full10Unsupported}
#[derive(Clone,Copy,Debug)]pub enum Chroma{TopLeftPoint420,CenteredBox420Diagnostic}
#[derive(Clone,Copy,Debug)]pub struct RpuFixed{
 pub profile:u8,pub explicit_dm:bool,pub compressed_dm:bool,pub signal_eotf:u16,pub eotf_params:[u32;3],pub bit_depth:u8,pub color_space:u8,pub chroma_format:u8,pub full_range:u8,
 pub ycc_q13:[i16;9],pub offset_q28:[u32;3],pub lms_q14:[i16;9],pub source_min_pq:u16,pub source_max_pq:u16,
}
#[derive(Clone,Copy,Debug)]pub struct Prepared{nonlinear:[[f64;3];3],offsets:[f64;3],lms:[[f64;3];3]}
#[derive(Clone,Copy,Debug)]pub struct Decoded{pub nonlinear:[f64;3],pub eotf_nits:[f64;3],pub linear_bt2020_nits:[f64;3],pub pq_clamped_mask:u8}
#[derive(Clone,Copy,Debug)]pub struct Encoded{pub pq_rgb:[f64;3],pub ycbcr:[f64;3],pub narrow10:[u16;3],pub gamut_clipped_mask:u8}
#[derive(Clone,Copy,Debug)]pub struct Block420{pub y:[u16;4],pub cb:u16,pub cr:u16,pub gamut_masks:[u8;4],pub chroma_location_type:u8,pub diagnostic_filter:bool}
const HPE:[[f64;3];3]=[[3.06441879,-2.16597676,0.10155818],[-0.65612108,1.78554118,-0.12943749],[0.01736321,-0.04725154,1.03004253]];
const M1:f64=2610./16384.;const M2:f64=2523./32.;const C1:f64=3424./4096.;const C2:f64=2413./128.;const C3:f64=2392./128.;
fn mat(m:[[f64;3];3],v:[f64;3])->[f64;3]{m.map(|r|r[0]*v[0]+r[1]*v[1]+r[2]*v[2])}
fn determinant(m:[[f64;3];3])->f64{let[a,b,c]=m[0];let[d,e,f]=m[1];let[g,h,i]=m[2];a*(e*i-f*h)-b*(d*i-f*g)+c*(d*h-e*g)}
fn finite(v:[f64;3])->Result<(),Error>{if v.iter().any(|x|!x.is_finite()){Err(Error::NonFinite)}else{Ok(())}}
impl RpuFixed{pub fn prepare(self)->Result<Prepared,Error>{
 if self.profile!=7||!self.explicit_dm||self.compressed_dm||self.signal_eotf!=65535||self.eotf_params!=[0;3]||!(10..=16).contains(&self.bit_depth)||self.color_space!=0||self.chroma_format!=0||self.full_range!=1{return Err(Error::MetadataTuple)}
 // Conservative supported offset domain, not a claim that other u32 values are invalid RPUs.
 if self.offset_q28.iter().any(|x|*x>1<<28){return Err(Error::OffsetDomain)}
 if self.source_min_pq>self.source_max_pq||self.source_max_pq>4095{return Err(Error::LuminanceBounds)}
 let mut nonlinear=[[0.;3];3];let mut lms=[[0.;3];3];for r in 0..3{for c in 0..3{nonlinear[r][c]=self.ycc_q13[r*3+c]as f64/8192.;lms[r][c]=self.lms_q14[r*3+c]as f64/16384.;}}
 if determinant(nonlinear).abs()<1e-12||determinant(lms).abs()<1e-12{return Err(Error::SingularMatrix)}
 Ok(Prepared{nonlinear,lms,offsets:self.offset_q28.map(|x|x as f64/268435456.)})
}}
pub fn pq_eotf_nits(x:f64)->Result<f64,Error>{if !x.is_finite(){return Err(Error::NonFinite)}if !(0.0..=1.0).contains(&x){return Err(Error::PqDomain)}let p=x.powf(1./M2);let denominator=C2-C3*p;if denominator<=0.{return Err(Error::PqDomain)}let n=((p-C1).max(0.)/denominator).powf(1./M1)*10000.;if !n.is_finite(){return Err(Error::NonFinite)}Ok(n)}
pub fn pq_inverse_from_nits(n:f64)->Result<f64,Error>{if !n.is_finite(){return Err(Error::NonFinite)}if !(0.0..=10000.0).contains(&n){return Err(Error::GamutDomain)}let p=(n/10000.).powf(M1);Ok(((C1+C2*p)/(1.+C3*p)).powf(M2))}
impl Prepared{
 /// Three co-sited component samples. This does not upsample a planar420 frame.
 pub fn decode_raw16(self,ycc:[u16;3],policy:PqPolicy)->Result<Decoded,Error>{self.decode_normalized(ycc.map(|v|v as f64/65536.),policy)}
 pub fn decode_normalized(self,ycc:[f64;3],policy:PqPolicy)->Result<Decoded,Error>{finite(ycc)?;if ycc.iter().any(|v|!(0.0..=1.0).contains(v)){return Err(Error::InputDomain)}let v=[ycc[0]-self.offsets[0],ycc[1]-self.offsets[1],ycc[2]-self.offsets[2]];let nonlinear=mat(self.nonlinear,v);finite(nonlinear)?;let mut pq=nonlinear;let mut mask=0;for(i,x)in pq.iter_mut().enumerate(){if !(0.0..=1.0).contains(x){match policy{PqPolicy::Reject=>return Err(Error::PqDomain),PqPolicy::ClampDiagnostic=>{*x=x.clamp(0.,1.);mask|=1<<i;}}}}
 let eotf_nits=[pq_eotf_nits(pq[0])?,pq_eotf_nits(pq[1])?,pq_eotf_nits(pq[2])?];let linear=mat(HPE,mat(self.lms,eotf_nits));finite(linear)?;Ok(Decoded{nonlinear,eotf_nits,linear_bt2020_nits:linear,pq_clamped_mask:mask})
}}
fn quantize(x:f64)->Result<u16,Error>{if !x.is_finite(){return Err(Error::NonFinite)}// ITU2100-3 Table9 Round, positive code values: floor(x+0.5).
 let n=(x+0.5).floor();if !(4.0..=1019.0).contains(&n){return Err(Error::Quantization)}Ok(n as u16)}
fn quantize_ycc(v:[f64;3],range:Range)->Result<[u16;3],Error>{match range{Range::Full10Unsupported=>return Err(Error::UnsupportedRange),Range::Narrow10=>{}}finite(v)?;Ok([quantize(64.+876.*v[0])?,quantize(512.+896.*v[1])?,quantize(512.+896.*v[2])?])}
pub fn encode_bt2020_pq(nits:[f64;3],policy:GamutPolicy,range:Range)->Result<Encoded,Error>{
 if matches!(range,Range::Full10Unsupported){return Err(Error::UnsupportedRange)}finite(nits)?;let mut n=nits;let mut mask=0;for(i,x)in n.iter_mut().enumerate(){if !(0.0..=10000.0).contains(x){match policy{GamutPolicy::Reject=>return Err(Error::GamutDomain),GamutPolicy::ClipDiagnostic=>{*x=x.clamp(0.,10000.);mask|=1<<i;}}}}
 let rgb=[pq_inverse_from_nits(n[0])?,pq_inverse_from_nits(n[1])?,pq_inverse_from_nits(n[2])?];let y=0.2627*rgb[0]+0.6780*rgb[1]+0.0593*rgb[2];let ycbcr=[y,(rgb[2]-y)/1.8814,(rgb[0]-y)/1.4746];Ok(Encoded{pq_rgb:rgb,ycbcr,narrow10:quantize_ycc(ycbcr,range)?,gamut_clipped_mask:mask})
}
pub fn encode_block_2x2(pixels:[[f64;3];4],policy:GamutPolicy,chroma:Chroma,range:Range)->Result<Block420,Error>{
 let a=[encode_bt2020_pq(pixels[0],policy,range)?,encode_bt2020_pq(pixels[1],policy,range)?,encode_bt2020_pq(pixels[2],policy,range)?,encode_bt2020_pq(pixels[3],policy,range)?];let y=a.map(|v|v.narrow10[0]);let (cb,cr,location,diagnostic)=match chroma{Chroma::TopLeftPoint420=>(a[0].ycbcr[1],a[0].ycbcr[2],2,false),Chroma::CenteredBox420Diagnostic=>(a.iter().map(|v|v.ycbcr[1]).sum::<f64>()/4.,a.iter().map(|v|v.ycbcr[2]).sum::<f64>()/4.,1,true)};let codes=quantize_ycc([0.,cb,cr],range)?;Ok(Block420{y,cb:codes[1],cr:codes[2],gamut_masks:a.map(|v|v.gamut_clipped_mask),chroma_location_type:location,diagnostic_filter:diagnostic})
}
pub mod vectors;
pub fn fel_color_vector_count()->u32{vectors::CASES.len()as u32}
pub fn fel_color_vector_value(index:u32,field:u32)->f64{vectors::compute(index as usize).ok().and_then(|v|v.get(field as usize).copied()).unwrap_or(f64::NAN)}
pub fn probe_block(index:usize)->Result<[f64;8],Error>{let c=match index{0=>Chroma::TopLeftPoint420,1=>Chroma::CenteredBox420Diagnostic,_=>return Err(Error::InputDomain)};let b=encode_block_2x2([[1000.,0.,0.],[0.,1000.,0.],[0.,0.,1000.],[203.;3]],GamutPolicy::Reject,c,Range::Narrow10)?;Ok([b.y[0]as f64,b.y[1]as f64,b.y[2]as f64,b.y[3]as f64,b.cb as f64,b.cr as f64,b.chroma_location_type as f64,b.diagnostic_filter as u8 as f64])}
pub fn fel_color_block_count()->u32{2}
pub fn fel_color_block_value(index:u32,field:u32)->f64{probe_block(index as usize).ok().and_then(|v|v.get(field as usize).copied()).unwrap_or(f64::NAN)}
pub fn fel_color_probe_run()->u32{for i in 0..vectors::CASES.len(){if vectors::validate(i).is_err(){return 1;}}for i in 0..2{let Ok(actual)=probe_block(i)else{return 2;};if actual.iter().zip(vectors::BLOCK_EXPECTED[i]).any(|(a,b)|*a!=b as f64){return 2;}}0}
#[cfg(test)]mod tests{use super::*;
 #[test]fn independent_vectors(){for i in 0..vectors::CASES.len(){vectors::validate(i).unwrap();}assert_eq!(fel_color_probe_run(),0);}
 #[test]fn metadata_negatives(){let m=vectors::METADATA[0];for bad in [RpuFixed{profile:8,..m},RpuFixed{explicit_dm:false,..m},RpuFixed{compressed_dm:true,..m},RpuFixed{signal_eotf:0,..m},RpuFixed{eotf_params:[1,0,0],..m},RpuFixed{bit_depth:8,..m},RpuFixed{color_space:1,..m},RpuFixed{chroma_format:1,..m},RpuFixed{full_range:0,..m}]{assert!(matches!(bad.prepare(),Err(Error::MetadataTuple)));}assert!(matches!(RpuFixed{offset_q28:[u32::MAX;3],..m}.prepare(),Err(Error::OffsetDomain)));assert!(matches!(RpuFixed{ycc_q13:[0;9],..m}.prepare(),Err(Error::SingularMatrix)));assert!(matches!(RpuFixed{lms_q14:[0;9],..m}.prepare(),Err(Error::SingularMatrix)));assert!(matches!(RpuFixed{source_min_pq:4000,source_max_pq:3000,..m}.prepare(),Err(Error::LuminanceBounds)));}
 #[test]fn explicit_domain_policies(){let p=vectors::METADATA[1].prepare().unwrap();assert!(p.decode_raw16([0;3],PqPolicy::Reject).is_err());let d=p.decode_raw16([0;3],PqPolicy::ClampDiagnostic).unwrap();assert_ne!(d.pq_clamped_mask,0);assert!(matches!(p.decode_normalized([f64::NAN,0.,0.],PqPolicy::Reject),Err(Error::NonFinite)));assert!(matches!(p.decode_normalized([-0.1,0.,0.],PqPolicy::Reject),Err(Error::InputDomain)));assert!(matches!(encode_bt2020_pq([-1.,100.,100.],GamutPolicy::Reject,Range::Narrow10),Err(Error::GamutDomain)));let e=encode_bt2020_pq([-1.,20000.,100.],GamutPolicy::ClipDiagnostic,Range::Narrow10).unwrap();assert_eq!(e.gamut_clipped_mask,3);assert!(matches!(encode_bt2020_pq([1.;3],GamutPolicy::Reject,Range::Full10Unsupported),Err(Error::UnsupportedRange)));assert!(matches!(pq_eotf_nits(1.1),Err(Error::PqDomain)));assert!(matches!(pq_inverse_from_nits(f64::INFINITY),Err(Error::NonFinite)));}
 #[test]fn anchors_quantization_and_chroma(){assert_eq!(pq_eotf_nits(0.).unwrap(),0.);assert!((pq_eotf_nits(1.).unwrap()-10000.).abs()<1e-8);for n in [0.,0.1,1.,100.,203.,1000.,10000.]{assert!((pq_eotf_nits(pq_inverse_from_nits(n).unwrap()).unwrap()-n).abs()<1e-6);}assert_eq!(quantize(64.5).unwrap(),65);assert_eq!(encode_bt2020_pq([0.;3],GamutPolicy::Reject,Range::Narrow10).unwrap().narrow10,[64,512,512]);let rgb=[[1000.,0.,0.],[0.,1000.,0.],[0.,0.,1000.],[203.;3]];let top=encode_block_2x2(rgb,GamutPolicy::Reject,Chroma::TopLeftPoint420,Range::Narrow10).unwrap();let center=encode_block_2x2(rgb,GamutPolicy::Reject,Chroma::CenteredBox420Diagnostic,Range::Narrow10).unwrap();assert_eq!(top.chroma_location_type,2);assert_eq!(center.chroma_location_type,1);assert_ne!([top.cb,top.cr],[center.cb,center.cr]);assert!(center.diagnostic_filter);}
}
