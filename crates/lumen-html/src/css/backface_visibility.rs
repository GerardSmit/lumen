//! `backface-visibility: hidden` for an element whose own 3D transform turns
//! its plane away from the viewer.
use super::{extra_props, typed_transforms, Style};
use lumen_common::dom_geometry::Matrix;

impl Style {
    pub(crate) fn backface_hidden(&self) -> bool {
        extra_props::keyword(self, "backface-visibility") == Some("hidden")
    }

    /// The element's own transform maps its plane's normal to point away from
    /// the viewer, as for `rotateY(180deg)`. Two-dimensional transforms never do.
    pub(crate) fn transform_faces_away(&self, reference: [f64; 2]) -> bool {
        let Some(matrix) = self.transform_matrix_3d(reference) else {
            return false;
        };
        let inverse = matrix.inverse();
        inverse.values.iter().all(|value| value.is_finite()) && inverse.values[10] < 0.0
    }

    pub(crate) fn transform_matrix_3d(&self, reference: [f64; 2]) -> Option<Matrix> {
        let mut combined = Matrix::default();
        for value in self.individual_transforms.iter().flatten() {
            combined = combined.multiply(value.matrix(reference)?);
        }
        let normal = match self.relative_expressions.iter().find(|expression| expression.slot == 59) {
            Some(source) => typed_transforms::computed_matrix(&source.raw, Some(reference))?,
            None => Matrix::default(),
        };
        Some(combined.multiply(normal))
    }
}
