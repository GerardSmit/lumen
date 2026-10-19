//! Language-neutral homogeneous geometry used by native DOM geometry values.
//! Matrices are column-major, with unrestricted IEEE-754 components.
/// Integral quarter turns have exact sine/cosine coefficients. Preserve all
/// other angles through normal trigonometry; no tolerance-based snapping.
pub fn sin_cos_degrees(degrees:f64)->(f64,f64) {
    match degrees.rem_euclid(360.0) {
        0.0=>(0.0,1.0),90.0=>(1.0,0.0),180.0=>(0.0,-1.0),270.0=>(-1.0,0.0),
        _=>degrees.to_radians().sin_cos(),
    }
}

/// The compact legacy transform representation stores radians in f32. Only
/// exact representable quarter turns qualify; source admission must preserve
/// higher-precision angles which happen to round to this representation.
pub fn exact_quarter_turn_radians_f32(radians:f32)->Option<(f32,f32)> {
    let quarter=(radians/core::f32::consts::FRAC_PI_2).round();
    if !quarter.is_finite() || radians!=quarter*core::f32::consts::FRAC_PI_2{return None;}
    Some(match quarter.rem_euclid(4.0) {0.0=>(0.0,1.0),1.0=>(1.0,0.0),2.0=>(0.0,-1.0),3.0=>(-1.0,0.0),_=>return None})
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point(pub [f64; 4]);
impl Default for Point { fn default() -> Self { Self([0.0, 0.0, 0.0, 1.0]) } }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix { pub values: [f64; 16], pub is_2d: bool }
impl Default for Matrix {
    fn default() -> Self { Self { values: [1.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0, 0.0,0.0,0.0,1.0], is_2d: true } }
}
impl Matrix {
    pub fn from_affine(values: [f64; 6]) -> Self {
        let mut matrix = Self::default();
        for (index, value) in [0, 1, 4, 5, 12, 13].into_iter().zip(values) { matrix.values[index] = value; }
        matrix
    }
    pub fn is_identity(&self) -> bool { self.values == Self::default().values }
    pub fn has_2d_components(&self) -> bool {
        [2,3,6,7,8,9,11,14].into_iter().all(|index| self.values[index] == 0.0) && self.values[10] == 1.0 && self.values[15] == 1.0
    }
    /// Project the local z=0 plane only when homogeneous division is constant.
    /// Arbitrary 3D rotations/translations/scales can produce an affine plane;
    /// varying perspective requires a projective renderer and cannot be reduced.
    pub fn affine_plane_projection(&self)->Option<[f64;6]> {
        if self.values[3]!=0.0 || self.values[7]!=0.0 || self.values[15]==0.0 {return None;}
        let values=[0,1,4,5,12,13].map(|index|self.values[index]/self.values[15]);
        values.iter().all(|value|value.is_finite()).then_some(values)
    }
    pub fn set(&mut self, index: usize, value: f64) {
        self.values[index] = value;
        if !self.has_2d_components() { self.is_2d = false; }
    }
    pub fn multiply(self, other: Self) -> Self {
        let mut values = [0.0; 16];
        for column in 0..4 { for row in 0..4 { values[column * 4 + row] = (0..4).map(|k| self.values[k * 4 + row] * other.values[column * 4 + k]).sum(); } }
        Self { values, is_2d: self.is_2d && other.is_2d }
    }
    pub fn transform(self, point: Point) -> Point {
        Point(std::array::from_fn(|row| (0..4).map(|column| self.values[column * 4 + row] * point.0[column]).sum()))
    }
    pub fn translated(self, x: f64, y: f64, z: f64) -> Self {
        let mut translation = Self::default(); translation.values[12] = x; translation.values[13] = y; translation.values[14] = z; translation.is_2d = z == 0.0;
        self.multiply(translation)
    }
    pub fn scaled(self, x: f64, y: f64, z: f64, origin: Point) -> Self {
        let mut scale = Self::default(); scale.values[0] = x; scale.values[5] = y; scale.values[10] = z; scale.is_2d = z == 1.0;
        self.translated(origin.0[0],origin.0[1],origin.0[2]).multiply(scale).translated(-origin.0[0],-origin.0[1],-origin.0[2])
    }
    pub fn rotated_axis(self, x: f64, y: f64, z: f64, degrees: f64) -> Self {
        let is_2d=x==0.0&&y==0.0;
        let length = x.hypot(y).hypot(z);
        if length == 0.0 { return self; }
        let (x,y,z) = (x/length,y/length,z/length); let (s,c) = sin_cos_degrees(degrees); let t=1.0-c;
        let rotation = Self { values: [t*x*x+c,t*x*y+s*z,t*x*z-s*y,0.0, t*x*y-s*z,t*y*y+c,t*y*z+s*x,0.0, t*x*z+s*y,t*y*z-s*x,t*z*z+c,0.0, 0.0,0.0,0.0,1.0], is_2d };
        self.multiply(rotation)
    }
    pub fn rotated(self, x: f64, y: f64, z: f64) -> Self {
        let mut result=self.rotated_axis(0.0,0.0,1.0,z).rotated_axis(0.0,1.0,0.0,y).rotated_axis(1.0,0.0,0.0,x);
        result.is_2d=self.is_2d && x==0.0 && y==0.0;
        result
    }
    pub fn rotated_from_vector(self,x:f64,y:f64)->Self {
        self.rotated_axis(0.0,0.0,1.0,if x==0.0&&y==0.0 {0.0}else{y.atan2(x).to_degrees()})
    }
    pub fn skewed(self, x: f64, y: f64) -> Self { let mut skew=Self::default(); skew.values[4]=x.to_radians().tan(); skew.values[1]=y.to_radians().tan(); self.multiply(skew) }
    pub fn inverse(self) -> Self {
        let mut rows = [[0.0;8];4];
        for row in 0..4 { for column in 0..4 { rows[row][column]=self.values[column*4+row]; } rows[row][row+4]=1.0; }
        for column in 0..4 {
            let pivot = (column..4).max_by(|&a,&b| rows[a][column].abs().total_cmp(&rows[b][column].abs())).unwrap();
            if rows[pivot][column] == 0.0 || !rows[pivot][column].is_finite() { return Self { values:[f64::NAN;16],is_2d:false }; }
            rows.swap(column,pivot); let divisor=rows[column][column];
            for value in &mut rows[column] { *value /= divisor; }
            for row in 0..4 { if row != column { let multiplier=rows[row][column]; for index in 0..8 { rows[row][index] -= multiplier*rows[column][index]; } } }
        }
        Self { values: std::array::from_fn(|index| rows[index%4][index/4+4]), is_2d:self.is_2d }
    }
}

/// Decomposed homogeneous geometry shared by every transform consumer.
/// A zero scale or nonfinite matrix has no continuous decomposition.
#[derive(Clone,Copy)]
struct Decomposed {
    translation:[f64;3],scale:[f64;3],skew:[f64;3],perspective:[f64;4],rotation:[f64;4],
}
fn dot(a:[f64;3],b:[f64;3])->f64 {(0..3).map(|i|a[i]*b[i]).sum()}
fn cross(a:[f64;3],b:[f64;3])->[f64;3] {[a[1]*b[2]-a[2]*b[1],a[2]*b[0]-a[0]*b[2],a[0]*b[1]-a[1]*b[0]]}
fn normalized(mut v:[f64;3])->Option<([f64;3],f64)> {
    let length=v[0].hypot(v[1]).hypot(v[2]);
    if length==0.0 || !length.is_finite(){return None;}
    for x in &mut v {*x/=length;}
    Some((v,length))
}
fn quaternion(columns:[[f64;3];3])->[f64;4] {
    // CSS Transforms' decomposition chooses a nonnegative scalar component.
    // The antisymmetric entries choose the three vector component signs.
    let diagonal=[columns[0][0],columns[1][1],columns[2][2]];
    let mut result=[
        0.5*(1.0+diagonal[0]-diagonal[1]-diagonal[2]).max(0.0).sqrt(),
        0.5*(1.0-diagonal[0]+diagonal[1]-diagonal[2]).max(0.0).sqrt(),
        0.5*(1.0-diagonal[0]-diagonal[1]+diagonal[2]).max(0.0).sqrt(),
        0.5*(1.0+diagonal[0]+diagonal[1]+diagonal[2]).max(0.0).sqrt(),
    ];
    if columns[2][1]>columns[1][2]{result[0]=-result[0];}
    if columns[0][2]>columns[2][0]{result[1]=-result[1];}
    if columns[1][0]>columns[0][1]{result[2]=-result[2];}
    result
}
fn rotation_axis_angle(q:[f64;4])->[f64;4]{
    let length=libm::sqrt(q[0]*q[0]+q[1]*q[1]+q[2]*q[2]);
    if length==0.0{[0.0,0.0,1.0,0.0]}else{[q[0]/length,q[1]/length,q[2]/length,2.0*libm::atan2(length,q[3])*180.0/core::f64::consts::PI]}
}
fn quaternion_product(a:[f64;4],b:[f64;4])->[f64;4] {
    let axis=cross([a[0],a[1],a[2]],[b[0],b[1],b[2]]);
    [a[3]*b[0]+b[3]*a[0]+axis[0],a[3]*b[1]+b[3]*a[1]+axis[1],a[3]*b[2]+b[3]*a[2]+axis[2],a[3]*b[3]-dot([a[0],a[1],a[2]],[b[0],b[1],b[2]])]
}
fn quaternion_power(q:[f64;4],count:f64)->[f64;4] {
    let sine=q[0].hypot(q[1]).hypot(q[2]);
    if sine==0.0{return [0.0,0.0,0.0,1.0];}
    // atan2 retains tiny rotations when cosine has rounded to exactly one.
    let angle=sine.atan2(q[3]);
    let ratio=(angle*count).sin()/sine;
    [q[0]*ratio,q[1]*ratio,q[2]*ratio,(angle*count).cos()]
}
fn quaternion_interpolate(a:[f64;4],b:[f64;4],progress:f64)->[f64;4] {
    let product=(0..4).map(|i|a[i]*b[i]).sum::<f64>().clamp(-1.0,1.0);
    // Preserve the signed dot product prescribed by CSS Transforms.
    if product.abs()==1.0{return a;}
    let angle=product.acos();let right=(progress*angle).sin()/(1.0-product*product).sqrt();let left=((1.0-progress)*angle).sin()/(1.0-product*product).sqrt();
    core::array::from_fn(|i|a[i]*left+b[i]*right)
}
impl Decomposed {
    fn from_matrix(mut matrix:Matrix)->Option<Self> {
        let divisor=matrix.values[15];
        if divisor==0.0 || !matrix.values.iter().all(|x|x.is_finite()){return None;}
        for x in &mut matrix.values {*x/=divisor;}
        let mut affine=matrix;
        for index in [3,7,11]{affine.values[index]=0.0;}
        affine.values[15]=1.0;
        let inverse=affine.inverse();if !inverse.values.iter().all(|x|x.is_finite()){return None;}
        let rhs=[matrix.values[3],matrix.values[7],matrix.values[11],matrix.values[15]];
        let perspective=if rhs[..3].iter().any(|x|*x!=0.0){core::array::from_fn(|i|(0..4).map(|j|inverse.values[i*4+j]*rhs[j]).sum())}else{[0.0,0.0,0.0,1.0]};
        let mut columns:[[f64;3];3]=core::array::from_fn(|i|core::array::from_fn(|j|matrix.values[i*4+j]));
        let (x,sx)=normalized(columns[0])?;columns[0]=x;
        let xy=dot(columns[0],columns[1]);columns[1]=core::array::from_fn(|i|columns[1][i]-columns[0][i]*xy);
        let (y,sy)=normalized(columns[1])?;columns[1]=y;
        let xz=dot(columns[0],columns[2]);columns[2]=core::array::from_fn(|i|columns[2][i]-columns[0][i]*xz);
        let yz=dot(columns[1],columns[2]);columns[2]=core::array::from_fn(|i|columns[2][i]-columns[1][i]*yz);
        let (z,sz)=normalized(columns[2])?;columns[2]=z;
        let mut scale=[sx,sy,sz];
        if dot(columns[0],cross(columns[1],columns[2]))<0.0 {for i in 0..3 {scale[i]=-scale[i];columns[i]=columns[i].map(|x|-x);}}
        Some(Self{translation:[matrix.values[12],matrix.values[13],matrix.values[14]],scale,skew:[xy/sy,xz/sz,yz/sz],perspective,rotation:quaternion(columns)})
    }
    fn matrix(self)->Option<Matrix> {
        let mut matrix=Matrix::default();
        for i in 0..4 {matrix.values[i*4+3]=self.perspective[i];}
        matrix=matrix.translated(self.translation[0],self.translation[1],self.translation[2]);
        let [x,y,z,w]=self.rotation;
        let rotation=Matrix{values:[1.0-2.0*(y*y+z*z),2.0*(x*y+z*w),2.0*(x*z-y*w),0.0,2.0*(x*y-z*w),1.0-2.0*(x*x+z*z),2.0*(y*z+x*w),0.0,2.0*(x*z+y*w),2.0*(y*z-x*w),1.0-2.0*(x*x+y*y),0.0,0.0,0.0,0.0,1.0],is_2d:false};
        matrix=matrix.multiply(rotation);
        for (index,value) in [(9,self.skew[2]),(8,self.skew[1]),(4,self.skew[0])] {
            let mut shear=Matrix::default();shear.values[index]=value;matrix=matrix.multiply(shear);
        }
        matrix=matrix.scaled(self.scale[0],self.scale[1],self.scale[2],Point::default());
        matrix.is_2d=matrix.has_2d_components();
        matrix.values.iter().all(|x|x.is_finite()).then_some(matrix)
    }
}
impl Matrix {
    /// Rotation-only consumers share the homogeneous quaternion authority.
    /// This deliberately bypasses the affine decomposition of scale/skew.
    pub fn interpolate_rotation(self,to:Self,progress:f64)->Option<[f64;4]>{
        if !progress.is_finite(){return None;}
        let a=Decomposed::from_matrix(self)?.rotation;let b=Decomposed::from_matrix(to)?.rotation;
        Some(rotation_axis_angle(quaternion_interpolate(a,b,progress)))
    }
    pub fn accumulated_rotation(self,to:Self,count:f64)->Option<[f64;4]>{
        if !count.is_finite(){return None;}
        let a=Decomposed::from_matrix(self)?.rotation;let b=Decomposed::from_matrix(to)?.rotation;
        Some(rotation_axis_angle(quaternion_product(quaternion_power(a,count),b)))
    }
    /// Preserve the existing two-dimensional affine authority; otherwise use
    /// finite homogeneous decomposition and spherical quaternion interpolation.
    pub fn interpolate(self,to:Self,progress:f64)->Option<Self> {
        if !progress.is_finite(){return None;}
        if self.has_2d_components()&&to.has_2d_components(){
            return crate::affine::interpolate([self.values[0],self.values[1],self.values[4],self.values[5],self.values[12],self.values[13]],[to.values[0],to.values[1],to.values[4],to.values[5],to.values[12],to.values[13]],progress).map(Self::from_affine);
        }
        let a=Decomposed::from_matrix(self)?;let b=Decomposed::from_matrix(to)?;let mix=|x:f64,y:f64|x+(y-x)*progress;
        Decomposed{translation:core::array::from_fn(|i|mix(a.translation[i],b.translation[i])),scale:core::array::from_fn(|i|mix(a.scale[i],b.scale[i])),skew:core::array::from_fn(|i|mix(a.skew[i],b.skew[i])),perspective:core::array::from_fn(|i|mix(a.perspective[i],b.perspective[i])),rotation:quaternion_interpolate(a.rotation,b.rotation,progress)}.matrix()
    }
    /// Weighted accumulation composes rotation deltas and uses identity-relative
    /// scale and perspective parameters. It does not multiply scale factors.
    pub fn accumulate(self,to:Self,count:f64)->Option<Self> {
        if !count.is_finite(){return None;}
        if self.has_2d_components()&&to.has_2d_components(){
            return crate::affine::accumulate_weighted([self.values[0],self.values[1],self.values[4],self.values[5],self.values[12],self.values[13]],[to.values[0],to.values[1],to.values[4],to.values[5],to.values[12],to.values[13]],count).map(Self::from_affine);
        }
        let a=Decomposed::from_matrix(self)?;let b=Decomposed::from_matrix(to)?;
        Decomposed{translation:core::array::from_fn(|i|count*a.translation[i]+b.translation[i]),scale:core::array::from_fn(|i|count*(a.scale[i]-1.0)+b.scale[i]),skew:core::array::from_fn(|i|count*a.skew[i]+b.skew[i]),perspective:core::array::from_fn(|i|count*(a.perspective[i]-if i==3{1.0}else{0.0})+b.perspective[i]),rotation:quaternion_product(quaternion_power(a.rotation,count),b.rotation)}.matrix()
    }
}

pub fn encode_numbers<const N: usize>(values: [f64; N]) -> Vec<u8> { values.into_iter().flat_map(f64::to_le_bytes).collect() }
pub fn decode_numbers<const N: usize>(bytes: &[u8]) -> Option<[f64; N]> {
    if bytes.len() != N.checked_mul(8)? { return None; }
    Some(std::array::from_fn(|index| f64::from_le_bytes(bytes[index*8..index*8+8].try_into().expect("fixed length verified"))))
}

#[cfg(test)]
mod tests {
    #[test]
    fn exact_quarter_turns_preserve_nearby_nonzero_rotation_components() {
        use super::{sin_cos_degrees,exact_quarter_turn_radians_f32,Matrix};
        for (degrees,expected) in [(0.0,(0.0,1.0)),(90.0,(1.0,0.0)),(180.0,(0.0,-1.0)),(270.0,(-1.0,0.0)),(360.0,(0.0,1.0)),(-90.0,(-1.0,0.0)),(-180.0,(0.0,-1.0)),(450.0,(1.0,0.0))] {
            assert_eq!(sin_cos_degrees(degrees),expected);
            assert_eq!(exact_quarter_turn_radians_f32((degrees as f32/90.0)*core::f32::consts::FRAC_PI_2),Some((expected.0 as f32,expected.1 as f32)));
            let matrix=Matrix::default().rotated_axis(0.0,0.0,1.0,degrees);
            assert_eq!([matrix.values[0],matrix.values[1],matrix.values[4],matrix.values[5]],[expected.1,expected.0,-expected.0,expected.1]);
        }
        assert_eq!(sin_cos_degrees(360000000090.0),(1.0,0.0));
        assert_eq!(sin_cos_degrees(-360000000090.0),(-1.0,0.0));
        for degrees in [90.0-1e-10,90.0+1e-10,180.0-1e-10,180.0+1e-10] {
            let (s,c)=sin_cos_degrees(degrees);
            assert_eq!((s,c),degrees.to_radians().sin_cos());
            assert!(s!=0.0 && c!=0.0,"near-quarter angles retain both nonzero components");
        }
        for bits in [core::f32::consts::FRAC_PI_2.to_bits()-1,core::f32::consts::FRAC_PI_2.to_bits()+1] {
            assert_eq!(exact_quarter_turn_radians_f32(f32::from_bits(bits)),None);
        }
        assert!(sin_cos_degrees(f64::INFINITY).0.is_nan());
        assert!(exact_quarter_turn_radians_f32(f32::INFINITY).is_none());
    }

    #[test]
    fn affine_plane_projection_preserves_homogeneous_coordinates(){
        let matrix=super::Matrix::default().translated(3.0,4.0,5.0).rotated_axis(1.0,2.0,3.0,30.0).scaled(2.0,3.0,4.0,super::Point::default());
        let [a,b,c,d,e,f]=matrix.affine_plane_projection().unwrap();
        for(x,y)in [(0.0,0.0),(20.0,-30.0),(-5.0,7.0)]{
            let point=matrix.transform(super::Point([x,y,0.0,1.0]));
            assert!((a*x+c*y+e-point.0[0]/point.0[3]).abs()<1e-12);
            assert!((b*x+d*y+f-point.0[1]/point.0[3]).abs()<1e-12);
        }
        let mut perspective=matrix;perspective.values[3]=0.01;assert!(perspective.affine_plane_projection().is_none());
        perspective=matrix;perspective.values[15]=2.0;assert_eq!(perspective.affine_plane_projection().unwrap()[4],e/2.0);
    }
    use super::*;
    #[test] fn homogeneous_decomposition_round_trip_perspective_rotation_and_accumulation() {
        let mut perspective=Matrix::default();perspective.values[11]=-1.0/400.0;perspective.is_2d=false;
        let matrix=perspective.translated(7.0,-8.0,9.0).rotated_axis(1.0,2.0,3.0,87.0).scaled(-2.0,3.0,4.0,Point::default());
        let restored=Decomposed::from_matrix(matrix).unwrap().matrix().unwrap();
        for(actual,expected)in restored.values.into_iter().zip(matrix.values.map(|value|value/matrix.values[15])){assert!((actual-expected).abs()<1e-10,"{actual} != {expected}");}
        let a=Matrix::default().translated(10.0,20.0,30.0).scaled(2.0,3.0,4.0,Point::default());
        let b=Matrix::default().translated(20.0,30.0,40.0).scaled(3.0,4.0,5.0,Point::default());
        let middle=a.interpolate(b,0.5).unwrap();assert_eq!([middle.values[12],middle.values[13],middle.values[14]],[15.0,25.0,35.0]);assert_eq!([middle.values[0],middle.values[5],middle.values[10]],[2.5,3.5,4.5]);
        let accumulated=a.accumulate(b,2.0).unwrap();assert_eq!([accumulated.values[12],accumulated.values[13],accumulated.values[14]],[40.0,70.0,100.0]);assert_eq!([accumulated.values[0],accumulated.values[5],accumulated.values[10]],[5.0,8.0,11.0]);
        assert!(Matrix::default().scaled(0.0,1.0,2.0,Point::default()).interpolate(b,0.5).is_none());
        let signed_dot=Matrix::default().rotated_axis(1.0,0.0,0.0,170.0).interpolate(Matrix::default().rotated_axis(1.0,0.0,0.0,-170.0),0.5).unwrap().transform(Point([0.0,1.0,0.0,1.0]));
        assert!((signed_dot.0[1]-1.0).abs()<1e-12&&signed_dot.0[2].abs()<1e-12);
        assert_eq!(quaternion_interpolate([1.0,0.0,0.0,0.0],[-1.0,0.0,0.0,0.0],0.5),[1.0,0.0,0.0,0.0]);
        let q=quaternion_interpolate([0.8,0.0,0.0,0.6],[-0.8,0.0,0.0,0.6],0.5);assert!(q[0].abs()<1e-12&&(q[3]-1.0).abs()<1e-12);
        let tiny=quaternion_power([1e-12,0.0,0.0,1.0],2.0);assert!((tiny[0]-2e-12).abs()<1e-24);
        assert_eq!(matrix.interpolate(matrix,1.0).unwrap().values,restored.values);
    }
    #[test] fn composition_inverse_and_homogeneous_points() {
        let matrix=Matrix::default().translated(4.0,5.0,6.0).scaled(2.0,3.0,4.0,Point::default());
        assert_eq!(matrix.transform(Point([1.0,2.0,3.0,1.0])),Point([6.0,11.0,18.0,1.0]));
        assert_eq!(matrix.transform(Point([1.0,2.0,3.0,0.0])),Point([2.0,6.0,12.0,0.0]));
        let restored=matrix.inverse().transform(matrix.transform(Point([1.0,2.0,3.0,1.0])));
        for (actual,expected) in restored.0.into_iter().zip([1.0,2.0,3.0,1.0]) { assert!((actual-expected).abs()<1e-12); }
        assert!(!matrix.is_2d); assert!(Matrix::from_affine([1.0,0.0,0.0,1.0,0.0,0.0]).is_identity());
    }
    #[test] fn rotation_singular_inverse_and_scalar_bits() {
        let rotated=Matrix::default().rotated_axis(0.0,0.0,1.0,90.0).transform(Point([1.0,0.0,0.0,1.0]));
        assert!(rotated.0[0].abs()<1e-12); assert!((rotated.0[1]-1.0).abs()<1e-12);
        assert!(!Matrix::default().rotated_axis(1.0,0.0,0.0,0.0).is_2d);
        assert!(Matrix::default().rotated(0.0,0.0,90.0).is_2d);
        assert!(Matrix::default().rotated_from_vector(-0.0,-0.0).is_identity());
        assert!(Matrix::default().scaled(0.0,1.0,1.0,Point::default()).inverse().values.iter().all(|value|value.is_nan()));
        let values=[-0.0,f64::INFINITY,f64::NEG_INFINITY,f64::from_bits(0x7ff800000000002a)];
        let decoded=decode_numbers::<4>(&encode_numbers(values)).unwrap();
        assert_eq!(values.map(f64::to_bits),decoded.map(f64::to_bits)); assert!(decode_numbers::<4>(&[0;31]).is_none());
    }
}
