//! Shared bounded SVG path geometry for HTML, Canvas and raster consumers.
use tiny_skia_path::{BoundedPathBuilder,Path,PathBuilder,Stroke};

const MAX_SVG_PATH_BYTES: usize = 1024 * 1024;
const MAX_SVG_PATH_SEGMENTS: usize = 65_536;

#[derive(Clone)]
pub struct ParsedSvgPath {
    pub path: Option<Path>,
    pub current: (f32, f32),
    pub subpath_start: (f32, f32),
    pub has_subpath: bool,
}

impl ParsedSvgPath {
    /// Exact curve extrema use the maintained path authority rather than the
    /// control-point envelope used for raster crop acceleration.
    pub fn object_bounds(&self)->Option<[f32;4]> {
        if !self.has_subpath {return None;}
        let Some(path)=self.path.as_ref() else{return Some([self.current.0,self.current.1,0.0,0.0]);};
        let bounds=path.compute_tight_bounds()?;
        Some([bounds.x(),bounds.y(),bounds.width(),bounds.height()])
    }
    /// SVG's supported initial stroke geometry is butt/miter, with miter limit
    /// four. The maintained stroker includes joins instead of inflating fills.
    pub fn stroke_bounds(&self,width:f32)->Option<[f32;4]> {
        if !width.is_finite() || width<0.0 {return None;}
        if width==0.0 {return self.object_bounds();}
        let path=self.path.as_ref()?.stroke(&Stroke{width,..Stroke::default()},1.0)?;
        let bounds=path.compute_tight_bounds()?;
        Some([bounds.x(),bounds.y(),bounds.width(),bounds.height()])
    }
}

/// Syntax invalidity is distinct from an exhausted geometry resource budget.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum SvgPathError { Invalid(&'static str),BudgetExceeded }
impl From<&'static str> for SvgPathError {fn from(value:&'static str)->Self{Self::Invalid(value)}}
impl SvgPathError {
    pub fn message(self)->&'static str{match self{Self::Invalid(value)=>value,Self::BudgetExceeded=>"SVG path geometry budget exceeded"}}
}

/// Parses SVG path data into the shared vector path representation used by
/// Canvas and the HTML image rasterizer.
pub fn parse_svg_path(data: &str) -> Result<ParsedSvgPath, &'static str> {
    if data.len() > MAX_SVG_PATH_BYTES {
        return Err("SVG path data is too large");
    }
    SvgPathParser::new(data).parse().map_err(SvgPathError::message)
}
/// Parse using the canonical SVG grammar and the maintained bounded builder.
/// The limit covers expanded arc segments before their buffers grow.
pub fn parse_svg_path_bounded(data:&str,max_live_bytes:usize)->Result<ParsedSvgPath,SvgPathError>{
    if data.len()>MAX_SVG_PATH_BYTES{return Err(SvgPathError::Invalid("SVG path data is too large"));}
    let builder=BoundedPathBuilder::new(max_live_bytes).map_err(|_|SvgPathError::BudgetExceeded)?;
    SvgPathParser::with_builder(data,builder).parse()
}


trait SvgPathBuilder: Sized {
    fn move_to(&mut self, x:f32, y:f32);
    fn line_to(&mut self, x:f32, y:f32);
    fn quad_to(&mut self, x1:f32, y1:f32, x:f32, y:f32);
    fn cubic_to(&mut self, x1:f32, y1:f32, x2:f32, y2:f32, x:f32, y:f32);
    fn close(&mut self);
    fn budget_exceeded(&self)->bool;
    fn finish_path(self)->Result<Option<Path>,SvgPathError>;
}
impl SvgPathBuilder for PathBuilder {
    fn move_to(&mut self,x:f32,y:f32){PathBuilder::move_to(self,x,y);}
    fn line_to(&mut self,x:f32,y:f32){PathBuilder::line_to(self,x,y);}
    fn quad_to(&mut self,x1:f32,y1:f32,x:f32,y:f32){PathBuilder::quad_to(self,x1,y1,x,y);}
    fn cubic_to(&mut self,x1:f32,y1:f32,x2:f32,y2:f32,x:f32,y:f32){PathBuilder::cubic_to(self,x1,y1,x2,y2,x,y);}
    fn close(&mut self){PathBuilder::close(self);}
    fn budget_exceeded(&self)->bool{false}
    fn finish_path(self)->Result<Option<Path>,SvgPathError>{Ok(self.finish())}
}
impl SvgPathBuilder for BoundedPathBuilder {
    fn move_to(&mut self,x:f32,y:f32){BoundedPathBuilder::move_to(self,x,y);}
    fn line_to(&mut self,x:f32,y:f32){BoundedPathBuilder::line_to(self,x,y);}
    fn quad_to(&mut self,x1:f32,y1:f32,x:f32,y:f32){BoundedPathBuilder::quad_to(self,x1,y1,x,y);}
    fn cubic_to(&mut self,x1:f32,y1:f32,x2:f32,y2:f32,x:f32,y:f32){BoundedPathBuilder::cubic_to(self,x1,y1,x2,y2,x,y);}
    fn close(&mut self){BoundedPathBuilder::close(self);}
    fn budget_exceeded(&self)->bool{BoundedPathBuilder::budget_exceeded(self)}
    fn finish_path(self)->Result<Option<Path>,SvgPathError>{self.finish().map_err(|_|SvgPathError::BudgetExceeded)}
}

struct SvgPathParser<'a,B=PathBuilder> {
    data: &'a [u8],
    offset: usize,
    builder: B,
    current: (f32, f32),
    subpath: (f32, f32),
    previous_command: u8,
    cubic_control: (f32, f32),
    quad_control: (f32, f32),
    segments: usize,
    last_was_number: bool,
}

impl<'a> SvgPathParser<'a,PathBuilder> {
    fn new(data: &'a str) -> Self {Self::with_builder(data,PathBuilder::new())}
}
impl<'a,B:SvgPathBuilder> SvgPathParser<'a,B> {
    fn with_builder(data:&'a str,builder:B)->Self {
        Self {
            data: data.as_bytes(),
            offset: 0,
            builder,
            current: (0.0, 0.0),
            subpath: (0.0, 0.0),
            previous_command: 0,
            cubic_control: (0.0, 0.0),
            quad_control: (0.0, 0.0),
            segments: 0,
            last_was_number: false,
        }
    }

    fn parse(mut self) -> Result<ParsedSvgPath, SvgPathError> {
        let mut command = 0u8;
        let mut had_move = false;
        while self.has_more() {
            if self.builder.budget_exceeded(){return Err(SvgPathError::BudgetExceeded);}
            self.skip_whitespace();
            if self.offset >= self.data.len() {
                break;
            }
            if self.data[self.offset].is_ascii_alphabetic() {
                command = self.data[self.offset];
                self.offset += 1;
                self.last_was_number = false;
                if !matches!(
                    command.to_ascii_uppercase(),
                    b'M' | b'Z' | b'L' | b'H' | b'V' | b'C' | b'S' | b'Q' | b'T' | b'A'
                ) {
                    return Err("unsupported SVG path command".into());
                }
                if command.to_ascii_uppercase() == b'Z' {
                    if !had_move {
                        return Err("SVG path close has no subpath".into());
                    }
                    self.builder.close();
                    self.current = self.subpath;
                    self.previous_command = command;
                    command = 0;
                    self.segment()?;
                    continue;
                }
            } else if command == 0 {
                return Err("SVG path data must begin with a command".into());
            }

            let relative = command.is_ascii_lowercase();
            let upper = command.to_ascii_uppercase();
            if !had_move && upper != b'M' {
                return Err("SVG path must begin with moveto".into());
            }
            match upper {
                b'M' => {
                    let (x, y) = self.point(relative)?;
                    self.builder.move_to(x, y);
                    self.current = (x, y);
                    self.subpath = (x, y);
                    had_move = true;
                    self.previous_command = command;
                    self.segment()?;
                    // Subsequent pairs after moveto are implicit lineto.
                    command = if relative { b'l' } else { b'L' };
                }
                b'L' => {
                    let (x, y) = self.point(relative)?;
                    self.builder.line_to(x, y);
                    self.current = (x, y);
                    self.previous_command = command;
                    self.segment()?;
                }
                b'H' => {
                    let value = self.number()?;
                    let x = if relative {
                        self.current.0 + value
                    } else {
                        value
                    };
                    self.builder.line_to(x, self.current.1);
                    self.current.0 = x;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'V' => {
                    let value = self.number()?;
                    let y = if relative {
                        self.current.1 + value
                    } else {
                        value
                    };
                    self.builder.line_to(self.current.0, y);
                    self.current.1 = y;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'C' => {
                    let c1 = self.point(relative)?;
                    let c2 = self.point(relative)?;
                    let end = self.point(relative)?;
                    self.builder.cubic_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
                    self.current = end;
                    self.cubic_control = c2;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'S' => {
                    let c1 = if matches!(self.previous_command.to_ascii_uppercase(), b'C' | b'S') {
                        (
                            2.0 * self.current.0 - self.cubic_control.0,
                            2.0 * self.current.1 - self.cubic_control.1,
                        )
                    } else {
                        self.current
                    };
                    let c2 = self.point(relative)?;
                    let end = self.point(relative)?;
                    self.builder.cubic_to(c1.0, c1.1, c2.0, c2.1, end.0, end.1);
                    self.current = end;
                    self.cubic_control = c2;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'Q' => {
                    let control = self.point(relative)?;
                    let end = self.point(relative)?;
                    self.builder.quad_to(control.0, control.1, end.0, end.1);
                    self.current = end;
                    self.quad_control = control;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'T' => {
                    let control =
                        if matches!(self.previous_command.to_ascii_uppercase(), b'Q' | b'T') {
                            (
                                2.0 * self.current.0 - self.quad_control.0,
                                2.0 * self.current.1 - self.quad_control.1,
                            )
                        } else {
                            self.current
                        };
                    let end = self.point(relative)?;
                    self.builder.quad_to(control.0, control.1, end.0, end.1);
                    self.current = end;
                    self.quad_control = control;
                    self.previous_command = command;
                    self.segment()?;
                }
                b'A' => {
                    let rx = self.number()?.abs();
                    let ry = self.number()?.abs();
                    let rotation = self.number()?;
                    let large_arc = self.flag()?;
                    let sweep = self.flag()?;
                    let end = self.point(relative)?;
                    append_svg_arc(
                        &mut self.builder,
                        self.current,
                        end,
                        rx,
                        ry,
                        rotation,
                        large_arc,
                        sweep,
                    )?;
                    self.current = end;
                    self.previous_command = command;
                    self.segment()?;
                }
                _ => return Err("invalid SVG path command".into()),
            }
        }
        Ok(ParsedSvgPath {
            path: self.builder.finish_path()?,
            current: self.current,
            subpath_start: self.subpath,
            has_subpath: had_move,
        })
    }

    fn has_more(&self) -> bool {
        self.offset < self.data.len()
    }
    fn skip_whitespace(&mut self) {
        while self
            .data
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }
    fn number(&mut self) -> Result<f32, &'static str> {
        self.skip_whitespace();
        if self.data.get(self.offset) == Some(&b',') {
            if !self.last_was_number {
                return Err("SVG path comma is misplaced");
            }
            self.offset += 1;
            self.skip_whitespace();
            if self.offset >= self.data.len()
                || self.data[self.offset] == b','
                || self.data[self.offset].is_ascii_alphabetic()
            {
                return Err("SVG path comma has no following number");
            }
        }
        let start = self.offset;
        if self
            .data
            .get(self.offset)
            .is_some_and(|byte| *byte == b'+' || *byte == b'-')
        {
            self.offset += 1;
        }
        let mut digits = 0;
        while self.data.get(self.offset).is_some_and(u8::is_ascii_digit) {
            self.offset += 1;
            digits += 1;
        }
        if self.data.get(self.offset) == Some(&b'.') {
            self.offset += 1;
            while self.data.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            return Err("invalid SVG path number");
        }
        if self
            .data
            .get(self.offset)
            .is_some_and(|byte| *byte == b'e' || *byte == b'E')
        {
            self.offset += 1;
            if self
                .data
                .get(self.offset)
                .is_some_and(|byte| *byte == b'+' || *byte == b'-')
            {
                self.offset += 1;
            }
            let exponent_start = self.offset;
            while self.data.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if self.offset == exponent_start {
                return Err("invalid SVG path exponent");
            }
        }
        let text = core::str::from_utf8(&self.data[start..self.offset])
            .map_err(|_| "invalid SVG path number")?;
        let value = text.parse::<f32>().map_err(|_| "invalid SVG path number")?;
        if !value.is_finite() {
            return Err("SVG path number must be finite");
        }
        self.last_was_number = true;
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool, &'static str> {
        self.skip_whitespace();
        if self.data.get(self.offset) == Some(&b',') {
            if !self.last_was_number {
                return Err("SVG arc flag separator is misplaced");
            }
            self.offset += 1;
            self.skip_whitespace();
        }
        let value = match self.data.get(self.offset) {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err("invalid SVG arc flag"),
        };
        self.offset += 1;
        self.last_was_number = true;
        Ok(value)
    }
    fn point(&mut self, relative: bool) -> Result<(f32, f32), &'static str> {
        let x = self.number()?;
        let y = self.number()?;
        let point = if relative {
            (self.current.0 + x, self.current.1 + y)
        } else {
            (x, y)
        };
        if point.0.is_finite() && point.1.is_finite() {
            Ok(point)
        } else {
            Err("SVG path coordinate is out of range")
        }
    }
    fn segment(&mut self) -> Result<(), &'static str> {
        self.segments += 1;
        if self.segments > MAX_SVG_PATH_SEGMENTS {
            Err("SVG path has too many segments")
        } else {
            Ok(())
        }
    }
}

fn append_svg_arc<B:SvgPathBuilder>(
    builder: &mut B,
    start: (f32, f32),
    end: (f32, f32),
    rx: f32,
    ry: f32,
    rotation_degrees: f32,
    large_arc: bool,
    sweep: bool,
) -> Result<(), &'static str> {
    if start == end {
        return Ok(());
    }
    if rx == 0.0 || ry == 0.0 {
        builder.line_to(end.0, end.1);
        return Ok(());
    }
    let phi = rotation_degrees.to_radians();
    let phi=libm::fmodf(phi,core::f32::consts::TAU);
    let phi=if phi<0.0 {phi+core::f32::consts::TAU}else{phi};
    let (sin_phi, cos_phi) = (libm::sinf(phi),libm::cosf(phi));
    let dx = (start.0 - end.0) * 0.5;
    let dy = (start.1 - end.1) * 0.5;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;
    let mut rx = rx.abs();
    let mut ry = ry.abs();
    let lambda = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lambda > 1.0 {
        let scale = libm::sqrtf(lambda);
        rx *= scale;
        ry *= scale;
    }
    let numerator = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let denominator = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    if denominator == 0.0 || !denominator.is_finite() {
        return Err("invalid SVG arc geometry");
    }
    let sign = if large_arc == sweep { -1.0 } else { 1.0 };
    let coefficient = sign * libm::sqrtf(numerator / denominator);
    let cxp = coefficient * rx * y1p / ry;
    let cyp = -coefficient * ry * x1p / rx;
    let center = (
        cos_phi * cxp - sin_phi * cyp + (start.0 + end.0) * 0.5,
        sin_phi * cxp + cos_phi * cyp + (start.1 + end.1) * 0.5,
    );
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| libm::atan2f(ux * vy - uy * vx,ux * vx + uy * vy);
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let theta = libm::atan2f(uy,ux);
    let mut delta = angle(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= core::f32::consts::TAU;
    }
    if sweep && delta < 0.0 {
        delta += core::f32::consts::TAU;
    }
    if large_arc && delta.abs() < core::f32::consts::PI {
        delta += if sweep {
            core::f32::consts::TAU
        } else {
            -core::f32::consts::TAU
        };
    }
    if !large_arc && delta.abs() > core::f32::consts::PI {
        delta += if sweep {
            -core::f32::consts::TAU
        } else {
            core::f32::consts::TAU
        };
    }
    let count = libm::ceilf(delta.abs() / core::f32::consts::FRAC_PI_2).max(1.0) as usize;
    let step = delta / count as f32;
    for segment in 0..count {
        let a0 = theta + step * segment as f32;
        let a1 = a0 + step;
        let (s0, c0) = (libm::sinf(a0),libm::cosf(a0));
        let (s1, c1) = (libm::sinf(a1),libm::cosf(a1));
        let point = |c: f32, s: f32| {
            (
                center.0 + cos_phi * rx * c - sin_phi * ry * s,
                center.1 + sin_phi * rx * c + cos_phi * ry * s,
            )
        };
        let derivative = |c: f32, s: f32| {
            (
                -cos_phi * rx * s - sin_phi * ry * c,
                -sin_phi * rx * s + cos_phi * ry * c,
            )
        };
        let p0 = point(c0, s0);
        let p1 = point(c1, s1);
        let d0 = derivative(c0, s0);
        let d1 = derivative(c1, s1);
        let tangent = (4.0 / 3.0) * libm::tanf(step / 4.0);
        builder.cubic_to(
            p0.0 + tangent * d0.0,
            p0.1 + tangent * d0.1,
            p1.0 - tangent * d1.0,
            p1.1 - tangent * d1.1,
            p1.0,
            p1.1,
        );
    }
    Ok(())
}


/// Operation-owned source geometry. Group edges retain geometry, not merely
/// axis-aligned boxes, so a rotated curved child keeps its actual extrema.
pub struct SvgGeometryArena {
    nodes:crate::limits::BudgetedVec<GeometryNode>,
    budget:alloc::sync::Arc<crate::limits::ByteBudget>,
    max_nodes:usize,
}
struct GeometryNode {
    bounds:Option<([f32;4],[f32;4])>,
    source:GeometrySource,
}
enum GeometrySource {
    Path {
        fill:Option<Path>,stroke:Option<Path>,point:Option<(f32,f32)>,
        _fill_memory:crate::limits::ByteLease,
        _stroke_memory:Option<crate::limits::ByteLease>,
    },
    Cells(crate::limits::BudgetedVec<[f32;4]>),
    Group(crate::limits::BudgetedVec<([f32;6],usize)>),
}
#[derive(Clone,Copy,Debug)]
pub struct SvgGeometryLimit;
impl SvgGeometryArena {
    pub fn new(max_bytes:usize,max_nodes:usize)->Self {
        let budget=crate::limits::ByteBudget::new(max_bytes);
        Self{nodes:crate::limits::BudgetedVec::new(budget.clone(),max_nodes),budget,max_nodes}
    }
    pub fn bounds(&self,id:usize)->Option<([f32;4],[f32;4])>{self.nodes.as_slice().get(id)?.bounds}
    pub fn checked_bytes(&self)->usize{self.budget.reserved()}
    pub fn remaining_bytes(&self)->usize{self.budget.limit().saturating_sub(self.budget.reserved())}
    /// Shared operation budget for canonical producer scratch and source buffers.
    pub fn source_budget(&self)->alloc::sync::Arc<crate::limits::ByteBudget>{self.budget.clone()}
    pub fn reserve_resource(&self,bytes:usize)->Result<crate::limits::ByteLease,SvgGeometryLimit>{
        self.budget.reserve(bytes).ok_or(SvgGeometryLimit)
    }
    pub fn cell_buffer(&self)->crate::limits::BudgetedVec<[f32;4]>{
        crate::limits::BudgetedVec::new(self.budget.clone(),65_536)
    }
    pub fn child_buffer(&self)->crate::limits::BudgetedVec<([f32;6],usize)>{
        crate::limits::BudgetedVec::new(self.budget.clone(),self.max_nodes)
    }
    pub fn reservation_buffer(&self)->crate::limits::BudgetedVec<crate::limits::ByteLease>{
        crate::limits::BudgetedVec::new(self.budget.clone(),self.max_nodes.saturating_mul(2))
    }
    fn insert(&mut self,source:GeometrySource,bounds:Option<([f32;4],[f32;4])>)->Result<usize,SvgGeometryLimit>{
        let id=self.nodes.len();self.nodes.push(GeometryNode{source,bounds}).map_err(|_|SvgGeometryLimit)?;Ok(id)
    }
    pub fn path_data(&mut self,data:&str,stroke_width:Option<f32>)->Result<Option<usize>,SvgGeometryLimit>{
        self.path_data_at_resolution(data,stroke_width,1.0)
    }
    /// Raster consumers use the same maintained outline at their actual
    /// device resolution; object geometry callers retain resolution one.
    pub fn path_data_at_resolution(&mut self,data:&str,stroke_width:Option<f32>,resolution:f32)->Result<Option<usize>,SvgGeometryLimit>{
        if !resolution.is_finite() || resolution<=0.0 {return Err(SvgGeometryLimit);}
        let available=self.remaining_bytes();let mut memory=self.reserve_resource(available)?;
        let parsed=match parse_svg_path_bounded(data,available){
            Ok(value)=>value,Err(SvgPathError::Invalid(_))=>return Ok(None),Err(SvgPathError::BudgetExceeded)=>return Err(SvgGeometryLimit),
        };
        if !memory.shrink_to(parsed.path.as_ref().map_or(0,Path::allocated_bytes)){return Err(SvgGeometryLimit);}
        self.path_with_memory(parsed,stroke_width,resolution,memory).map(Some)
    }
    pub fn path(&mut self,parsed:ParsedSvgPath,stroke_width:Option<f32>)->Result<usize,SvgGeometryLimit>{
        let memory=self.reserve_resource(parsed.path.as_ref().map_or(0,Path::allocated_bytes))?;
        self.path_with_memory(parsed,stroke_width,1.0,memory)
    }
    fn path_with_memory(&mut self,parsed:ParsedSvgPath,stroke_width:Option<f32>,resolution:f32,fill_memory:crate::limits::ByteLease)->Result<usize,SvgGeometryLimit>{
        let fill_bounds=parsed.object_bounds();
        let point=parsed.has_subpath.then_some(parsed.current);
        let fill=parsed.path;
        let mut stroke_memory=None;
        let stroke=if let (Some(path),Some(width))=(fill.as_ref(),stroke_width.filter(|width|*width>0.0)) {
            if fill_bounds.is_some_and(|bounds|bounds[2]!=0.0 || bounds[3]!=0.0) {
                // The maintained stroker charges its three builders and their
                // replacement-buffer peaks, excluding this borrowed fill.
                let available=self.remaining_bytes();let mut memory=self.reserve_resource(available)?;
                let result=path.stroke_bounded(&Stroke{width,..Stroke::default()},resolution,available).map_err(|_|SvgGeometryLimit)?;
                if let Some(path)=result.as_ref(){
                    if !memory.shrink_to(path.allocated_bytes()){return Err(SvgGeometryLimit);}
                    stroke_memory=Some(memory);
                }
                result
            }else{None}
        }else{None};
        let stroke_bounds=stroke.as_ref().and_then(Path::compute_tight_bounds).map(rect_array);
        let bounds=fill_bounds.map(|fill|(fill,join_bounds(Some(fill),stroke_bounds).unwrap_or(fill)));
        self.insert(GeometrySource::Path{fill,stroke,point,_fill_memory:fill_memory,_stroke_memory:stroke_memory},bounds)
    }
    pub fn cells(&mut self,cells:crate::limits::BudgetedVec<[f32;4]>)->Result<usize,SvgGeometryLimit>{
        let bounds=cells.as_slice().iter().try_fold(None,|old,&rect|{
            tiny_skia_path::Rect::from_xywh(rect[0],rect[1],rect[2],rect[3]).ok_or(SvgGeometryLimit)?;
            Ok::<_,SvgGeometryLimit>(join_bounds(old,Some(rect)))
        })?;
        self.insert(GeometrySource::Cells(cells),bounds.map(|bounds|(bounds,bounds)))
    }
    pub fn group(&mut self,children:crate::limits::BudgetedVec<([f32;6],usize)>)->Result<usize,SvgGeometryLimit>{
        let mut fill=None;let mut stroke=None;let mut visits=0;
        for &(matrix,id) in children.as_slice() {
            let transform=geometry_transform(matrix);
            if !transform.is_finite(){return Err(SvgGeometryLimit);}
            if let Some((next_fill,next_stroke))=self.transformed_bounds(id,transform,0,&mut visits)? {
                fill=join_bounds(fill,Some(next_fill));stroke=join_bounds(stroke,Some(next_stroke));
            }
        }
        self.insert(GeometrySource::Group(children),fill.zip(stroke))
    }
    /// Project retained fill and stroke geometry through its real affine
    /// transform, including curve extrema, under this operation's lease.
    pub fn projected_bounds(&self,id:usize,matrix:[f32;6])->Result<Option<([f32;4],[f32;4])>,SvgGeometryLimit>{
        let transform=geometry_transform(matrix);
        if !transform.is_finite(){return Err(SvgGeometryLimit);}
        self.transformed_bounds(id,transform,0,&mut 0)
    }
    fn transformed_bounds(&self,id:usize,transform:tiny_skia_path::Transform,depth:usize,visits:&mut usize)
        ->Result<Option<([f32;4],[f32;4])>,SvgGeometryLimit> {
        *visits=visits.checked_add(1).ok_or(SvgGeometryLimit)?;
        if depth>512 || *visits>65_536{return Err(SvgGeometryLimit);}
        let node=self.nodes.as_slice().get(id).ok_or(SvgGeometryLimit)?;
        if transform.is_identity(){return Ok(node.bounds);}
        if !transform.has_skew() {
            return node.bounds.map(|(fill,stroke)|Ok((transform_rect(fill,transform)?,transform_rect(stroke,transform)?))).transpose();
        }
        match &node.source {
            GeometrySource::Path{fill,stroke,point,..}=>{
                let transformed=|path:&Path|->Result<Option<[f32;4]>,SvgGeometryLimit>{
                    let _temporary=self.reserve_resource(path.allocated_bytes())?;
                    let path=path.clone().transform(transform).ok_or(SvgGeometryLimit)?;
                    Ok(path.compute_tight_bounds().map(rect_array))
                };
                let fill=if let Some(path)=fill {transformed(path)?}else{None}.or_else(||point.map(|(x,y)|{
                    let mut point=tiny_skia_path::Point::from_xy(x,y);transform.map_point(&mut point);[point.x,point.y,0.0,0.0]
                }));
                let stroke=if let Some(path)=stroke {transformed(path)?}else{None};
                Ok(fill.map(|fill|(fill,join_bounds(Some(fill),stroke).unwrap_or(fill))))
            }
            GeometrySource::Cells(cells)=>{
                let mut bounds=None;
                for &cell in cells.as_slice() {bounds=join_bounds(bounds,Some(transform_rect(cell,transform)?));}
                Ok(bounds.map(|bounds|(bounds,bounds)))
            }
            GeometrySource::Group(children)=>{
                let mut fill=None;let mut stroke=None;
                for &(next,id) in children.as_slice() {
                    if let Some((a,b))=self.transformed_bounds(id,transform.pre_concat(geometry_transform(next)),depth+1,visits)? {
                        fill=join_bounds(fill,Some(a));stroke=join_bounds(stroke,Some(b));
                    }
                }
                Ok(fill.zip(stroke))
            }
        }
    }
}
fn geometry_transform(matrix:[f32;6])->tiny_skia_path::Transform{
    tiny_skia_path::Transform::from_row(matrix[0],matrix[1],matrix[2],matrix[3],matrix[4],matrix[5])
}
fn rect_array(rect:tiny_skia_path::Rect)->[f32;4]{[rect.x(),rect.y(),rect.width(),rect.height()]}
fn transform_rect(rect:[f32;4],transform:tiny_skia_path::Transform)->Result<[f32;4],SvgGeometryLimit>{
    use tiny_skia_path::{Rect,Point};
    let mut corners=[Point::from_xy(rect[0],rect[1]),Point::from_xy(rect[0]+rect[2],rect[1]),
        Point::from_xy(rect[0]+rect[2],rect[1]+rect[3]),Point::from_xy(rect[0],rect[1]+rect[3])];
    transform.map_points(&mut corners);Rect::from_points(&corners).map(rect_array).ok_or(SvgGeometryLimit)
}
fn join_bounds(a:Option<[f32;4]>,b:Option<[f32;4]>)->Option<[f32;4]>{
    use tiny_skia_path::{Rect,Point};
    match (a,b){(None,value)|(value,None)=>value,(Some(a),Some(b))=>{
        Rect::from_points(&[Point::from_xy(a[0],a[1]),Point::from_xy(a[0]+a[2],a[1]+a[3]),
            Point::from_xy(b[0],b[1]),Point::from_xy(b[0]+b[2],b[1]+b[3])]).map(rect_array)
    }}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_svg_geometry_arena_projects_resolution_aware_stroke_sources() {
        let data="M3 9 C3 -4 18 -4 18 9 Q10 20 3 9Z";
        let matrix=[1.0,0.25,0.5,1.5,4.125,-2.625];
        for scale in [1.0,1.25,2.0,8.0] {
            let device=tiny_skia_path::Transform::from_row(matrix[0]*scale,matrix[1]*scale,matrix[2]*scale,matrix[3]*scale,0.0,0.0);
            let resolution=tiny_skia_path::PathStroker::compute_resolution_scale(&device);
            let mut arena=SvgGeometryArena::new(1024*1024,1);
            let id=arena.path_data_at_resolution(data,Some(2.0),resolution).unwrap().unwrap();
            let actual=arena.projected_bounds(id,matrix).unwrap().unwrap();
            // Independent retained-path stroker and transform oracle uses the
            // same public backend resolution as actual SVG rasterization.
            let path=parse_svg_path(data).unwrap().path.unwrap();
            let fill=path.clone().transform(geometry_transform(matrix)).unwrap().compute_tight_bounds().map(rect_array);
            let outline=path.stroke(&Stroke{width:2.0,..Stroke::default()},resolution).unwrap()
                .transform(geometry_transform(matrix)).unwrap().compute_tight_bounds().map(rect_array);
            assert_eq!(actual,(fill.unwrap(),join_bounds(fill,outline).unwrap()));
            assert!(arena.checked_bytes()<=1024*1024);
        }
        let mut arena=SvgGeometryArena::new(128,1);
        assert!(arena.path_data_at_resolution(data,Some(2.0),8.0).is_err());
        assert!(arena.checked_bytes()<=128);
    }

    #[test]
    fn shared_svg_geometry_arena_keeps_transformed_curve_sources_and_bounds_memory() {
        let mut arena=SvgGeometryArena::new(1024*1024,64);
        let source=arena.path(parse_svg_path("M0 0 Q10 20 20 0").unwrap(),None).unwrap();
        let n=core::f32::consts::FRAC_1_SQRT_2;
        let mut children=arena.child_buffer();children.push(([n,n,-n,n,0.0,0.0],source)).unwrap();
        let group=arena.group(children).unwrap();
        let (tight,_)=arena.bounds(group).unwrap();
        assert!(tight[0]>-2.0 && tight[0]< -1.0 && tight[1]+tight[3]<16.0,
            "rotated curves keep extrema rather than their rectangular envelope: {tight:?}");
        assert!(arena.checked_bytes()<=1024*1024);
        let mut tiny=SvgGeometryArena::new(64,1);
        assert!(tiny.path_data("M0 0 C80 90 -40 90 40 0",Some(1000.0)).is_err());
    }

    #[test]
    fn shared_svg_bounded_parser_preserves_source_and_reports_resource_failure() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<BoundedPathBuilder>();
        assert!(BoundedPathBuilder::new(0).is_err());
        assert_eq!(BoundedPathBuilder::new(1024).unwrap().finish().unwrap(), None);
        for source in ["M0 0 L40 0", "M0 0 Q20 80 40 0", "M0 0 C80 90 -40 90 40 0", "M0 0 A20 10 35 1 1 40 0 Z"] {
            let ordinary = parse_svg_path(source).unwrap();
            let bounded = parse_svg_path_bounded(source, 1024 * 1024).unwrap();
            assert_eq!(ordinary.path, bounded.path);
            assert_eq!(ordinary.current, bounded.current);
            assert_eq!(parse_svg_path_bounded(source, 1).err(), Some(SvgPathError::BudgetExceeded));
        }
        let mut builder = BoundedPathBuilder::new(64).unwrap();
        for n in 0..100 { builder.line_to(n as f32, 1.0); }
        assert!(builder.budget_exceeded());
        assert!(builder.finish().is_err());
        assert!(matches!(parse_svg_path_bounded("L1 2", 1024), Err(SvgPathError::Invalid(_))));
    }

    #[test]
    fn shared_svg_bounded_stroker_preserves_geometry_and_stops_at_budget() {
        use tiny_skia_path::{LineCap, LineJoin};
        fn send_sync<T: Send + Sync>() {}
        send_sync::<PathBuilder>();
        for source in ["M0 0 L40 0", "M0 0 Q20 80 40 0", "M0 0 C80 90 -40 90 40 0", "M0 0 A20 10 35 1 1 40 0 Z", "M0 0 L20 30 L40 0 Z"] {
            let parsed = parse_svg_path(source).unwrap();
            let path = parsed.path.as_ref().unwrap();
            let original = path.clone();
            for cap in [LineCap::Butt, LineCap::Round, LineCap::Square] {
                for join in [LineJoin::Miter, LineJoin::Round, LineJoin::Bevel] {
                    let stroke = Stroke { width: 6.0, line_cap: cap, line_join: join, ..Stroke::default() };
                    for scale in [1.0, 4.0] {
                        assert_eq!(path.stroke_bounded(&stroke, scale, 1024 * 1024).unwrap(), path.stroke(&stroke, scale));
                    }
                    for budget in [0, 40, 64, 128] {
                        assert!(path.stroke_bounded(&stroke, 1024.0, budget).is_err(), "{source} {cap:?} {join:?} {budget}");
                    }
                }
            }
            assert_eq!(path, &original);
            assert!(path.allocated_bytes() >= path.points().len() * core::mem::size_of::<tiny_skia_path::Point>());
        }
        let path = parse_svg_path("M0 0 L1 1").unwrap().path.unwrap();
        assert_eq!(path.stroke_bounded(&Stroke { width: 0.0, ..Stroke::default() }, 1.0, 0), Ok(None));
    }
    #[test]
    fn shared_svg_transform_reference_uses_curve_extrema_and_stroke_outline() {
        assert_eq!(parse_svg_path("").unwrap().object_bounds(),None);
        assert_eq!(parse_svg_path("M12 34").unwrap().object_bounds(),Some([12.0,34.0,0.0,0.0]));
        let curve=parse_svg_path("M0 0 Q10 20 20 0").unwrap();
        assert_eq!(curve.object_bounds(),Some([0.0,0.0,20.0,10.0]));
        let line=parse_svg_path("M10 10 L30 10").unwrap();
        assert_eq!(line.stroke_bounds(4.0),Some([10.0,8.0,20.0,4.0]));
        let corner=parse_svg_path("M0 10 L10 0 L20 10").unwrap();
        let bounds=corner.stroke_bounds(4.0).unwrap();
        assert!(bounds[1] < -2.0,"miter extends beyond a simple fill-box inflation: {bounds:?}");
        assert!(parse_svg_path("L10 10").is_err());
        assert!(parse_svg_path("M0 0 A1 1 0 2 0 4 4").is_err());
    }
}
