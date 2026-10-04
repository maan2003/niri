
struct Params {
    input: mat4x4<f32>,
    previous: mat4x4<f32>,
    next: mat4x4<f32>,
    // width, height, blend mode, reference luminance
    output: vec4<f32>,
    // resize progress, saturation, noise, mode (0 copy, 1 resize, 2 postprocess)
    effect: vec4<f32>,
    background: vec4<f32>,
    // frame position x/y; previous and next force opaque
    flags: vec4<f32>,
};
@group(0) @binding(0) var previous: texture_2d<f32>;
@group(0) @binding(1) var next: texture_2d<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@vertex fn vertex(@builtin(vertex_index) index:u32) -> @builtin(position) vec4<f32> {
    let x=f32((index << 1u) & 2u);
    let y=f32(index & 2u);
    return vec4<f32>(x*2.0-1.0, 1.0-y*2.0, 0.0, 1.0);
}
fn fetch(tex:texture_2d<f32>, uv:vec2<f32>, border:bool, nearest:bool)->vec4<f32> {
    let size=vec2<i32>(textureDimensions(tex));
    if(nearest) {
        return textureLoad(tex,clamp(vec2<i32>(floor(uv*vec2<f32>(size))),vec2<i32>(0),size-1),0);
    }
    let p=uv*vec2<f32>(size)-0.5;
    let lo=vec2<i32>(floor(p));
    let f=fract(p);
    var samples:array<vec4<f32>,4>;
    for(var y=0;y<2;y++) {
        for(var x=0;x<2;x++) {
            let q=lo+vec2<i32>(x,y);
            var c=textureLoad(tex,clamp(q,vec2<i32>(0),size-1),0);
            if(border && (any(q<vec2<i32>(0)) || any(q>=size))) { c=vec4<f32>(0.0); }
            samples[y*2+x]=c;
        }
    }
    return mix(mix(samples[0],samples[1],f.x),mix(samples[2],samples[3],f.x),f.y);
}
fn pq(v:vec3<f32>)->vec3<f32> {
    let y=pow(clamp(v,vec3<f32>(0.0),vec3<f32>(1.0)),vec3<f32>(0.1593017578125));
    return pow((0.8359375+18.8515625*y)/(1.0+18.6875*y),vec3<f32>(78.84375));
}
fn encode(c:vec4<f32>)->vec4<f32> {
    if(params.output.z==0.0){return c;}
    var rgb=c.rgb;
    if(c.a>0.0){rgb/=c.a;}
    rgb=pow(max(rgb,vec3<f32>(0.0)),vec3<f32>(2.2));
    if(params.output.z==1.0){
        rgb=vec3<f32>(dot(rgb,vec3<f32>(0.627404,0.329283,0.043313)),
                     dot(rgb,vec3<f32>(0.069097,0.919540,0.011362)),
                     dot(rgb,vec3<f32>(0.016391,0.088013,0.895595)));
        rgb=pq(rgb*params.output.w);
    }else{
        rgb=vec3<f32>(dot(rgb,vec3<f32>(0.822462,0.177538,0.0)),
                     dot(rgb,vec3<f32>(0.033194,0.966806,0.0)),
                     dot(rgb,vec3<f32>(0.017083,0.072397,0.910520)));
        rgb=pow(max(rgb,vec3<f32>(0.0)),vec3<f32>(1.0/2.2));
    }
    return vec4<f32>(rgb*c.a,c.a);
}
fn hash12(p:vec2<f32>)->f32 {
    var p3=fract(vec3<f32>(p.x,p.y,p.x)*0.1031);
    p3+=dot(p3,p3.yzx+33.33);
    return fract((p3.x+p3.y)*p3.z);
}
@fragment fn fragment(@builtin(position) pos:vec4<f32>)->@location(0) vec4<f32> {
    let uv=(params.input*vec4<f32>(pos.xy/params.output.xy,1.0,1.0)).xy;
    let prev_uv=(params.previous*vec4<f32>(uv,1.0,1.0)).xy;
    var c=fetch(previous,prev_uv,params.effect.w==1.0 || params.flags.z==-1.0,params.flags.w==-1.0);
    if(params.flags.z==1.0){c.a=1.0;}
    if(params.effect.w==1.0){
        let next_uv=(params.next*vec4<f32>(uv,1.0,1.0)).xy;
        var n=fetch(next,next_uv,true,false);
        if(params.flags.w==1.0){n.a=1.0;}
        c=mix(c,n,params.effect.x);
    }
    if(params.effect.w==2.0){
        c=vec4<f32>(mix(vec3<f32>(dot(c.rgb,vec3<f32>(0.2126,0.7152,0.0722))),c.rgb,params.effect.y),c.a);
        c=vec4<f32>(c.rgb+(hash12(pos.xy+params.flags.xy)-0.5)*params.effect.z,c.a);
        c+=params.background*(1.0-c.a);
    }
    return encode(c);
}
