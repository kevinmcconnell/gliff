// BT.709 limited range, matching haver-codec::color (the CPU reference).
float luma(vec3 c)   { return (16.0  + 0.1826 * c.r + 0.6142 * c.g + 0.0620 * c.b) / 255.0; }
float chroma_u(vec3 c) { return (128.0 - 0.1006 * c.r - 0.3386 * c.g + 0.4392 * c.b) / 255.0; }
float chroma_v(vec3 c) { return (128.0 + 0.4392 * c.r - 0.3989 * c.g - 0.0403 * c.b) / 255.0; }
vec3 to_rgb(float y8, float u8, float v8) {
    float y = y8 * 255.0 - 16.0;
    float cb = u8 * 255.0 - 128.0;
    float cr = v8 * 255.0 - 128.0;
    return clamp(vec3(1.1644 * y + 1.7927 * cr, 1.1644 * y - 0.2132 * cb - 0.5329 * cr, 1.1644 * y + 2.1124 * cb) / 255.0, 0.0, 1.0);
}
