#[test]
fn geometry_values_math_native_clone_and_intrinsic_reconstruction() {
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(|| {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        runtime.set_deadline(std::time::Duration::from_secs(10));
        let engine = runtime.engine();
        lumen_html_js::install(engine.ctx(), "<!doctype html><body></body>", 1024).unwrap();
        let result = engine.eval_value(r#"
            const check=(value,message)=>{if(!value)throw new Error(message)};
            const matrix=new DOMMatrix([2,0,0,3,4,5]);
            check(matrix instanceof DOMMatrixReadOnly && matrix.is2D,'native inheritance');
            const order=[],number=(name,value)=>({valueOf(){order.push(name);return value}});
            matrix.scale(number('x',2),number('y',3),number('z',1),number('ox',0),number('oy',0),number('oz',0));
            check(order.join(',')==='x,y,z,ox,oy,oz','WebIDL numeric conversion order');
            const point=matrix.transformPoint({x:2,y:3});
            check(point.x===8 && point.y===14 && point.w===1,'affine point');
            const back=matrix.inverse().transformPoint(point);
            check(Math.abs(back.x-2)<1e-10 && Math.abs(back.y-3)<1e-10,'inverse');
            check(matrix.translateSelf(1,2)===matrix && matrix.e===6 && matrix.f===11,'multiply order and this');
            matrix.m13=1;check(!matrix.is2D,'three dimensional mutation');
            const readonly=DOMMatrixReadOnly.fromMatrix({a:2,d:3,e:4,f:5});
            let conflict=false;try{DOMMatrix.fromMatrix({a:1,m11:2})}catch(e){conflict=e.name==='TypeError'}
            check(conflict,'dictionary alias conflict');
            const singular=new DOMMatrix([0,0,0,0,0,0]).inverse();
            check(Number.isNaN(singular.m11) && !singular.is2D,'singular inversion');
            const floating=DOMMatrix.fromFloat64Array(new Float64Array([1,0,0,1,-0,7]));
            check(Object.is(floating.e,-0) && floating.toFloat64Array().length===16,'intrinsic float conversion');
            const mutablePoint=new DOMPoint(1,2,3,4), readonlyPoint=new DOMPointReadOnly(5,6,7,8);
            mutablePoint.x='9';check(mutablePoint.x===9,'mutable numeric coercion');
            mutablePoint.x={valueOf(){mutablePoint.y=17;return 9}};
            check(mutablePoint.x===9 && mutablePoint.y===17,'reentrant point conversion preserves other fields');
            const reentrantMatrix=new DOMMatrix();
            reentrantMatrix.a={valueOf(){reentrantMatrix.b=19;return 3}};
            check(reentrantMatrix.a===3 && reentrantMatrix.b===19,'reentrant matrix conversion preserves other fields');
            const rect=new DOMRect(1,2,3,4),readonlyRect=new DOMRectReadOnly(5,6,7,8);
            let authored=0;Object.defineProperty(matrix,'m11',{get(){authored++;throw new Error('author getter')}});
            const originalConstructor=DOMMatrix;
            globalThis.DOMMatrix=function(){throw new Error('author constructor')};
            const cloned=structuredClone({matrix,again:matrix,readonly,mutablePoint,readonlyPoint,rect,readonlyRect,floating});
            check(authored===0 && cloned.matrix===cloned.again && cloned.matrix instanceof originalConstructor,'native snapshot and alias');
            check(cloned.matrix.m11===2 && cloned.matrix.m13===1 && !cloned.matrix.is2D,'matrix snapshot');
            check(cloned.readonly instanceof DOMMatrixReadOnly && !(cloned.readonly instanceof originalConstructor),'readonly brand');
            check(cloned.mutablePoint instanceof DOMPoint && cloned.mutablePoint.x===9 && cloned.readonlyPoint.w===8,'point brands');
            check(cloned.rect instanceof DOMRect && cloned.readonlyRect instanceof DOMRectReadOnly && cloned.readonlyRect.height===8,'rect brands');
            check(Object.is(cloned.floating.e,-0),'clone negative zero');
            true
        "#).unwrap();
        match result {
            Ok(lumen::embed::Value::Bool(true)) => {},
            Err(error) => panic!("geometry test: {}", engine.ctx().coerce_string(&error).unwrap_or_default()),
            _ => panic!("geometry assertions did not complete"),
        }
    }).unwrap().join().unwrap();
}
