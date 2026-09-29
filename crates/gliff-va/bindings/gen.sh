#!/bin/sh
# Regenerate src/bindings.rs from the installed libva headers. Needs
# bindgen-cli (`cargo install bindgen-cli`) and clang. The output is
# committed so the crate builds without either.
set -e
cd "$(dirname "$0")"
FUNCS='vaGetDisplayDRM|vaInitialize|vaTerminate|vaQueryVendorString|vaErrorStr'
FUNCS="$FUNCS|vaMaxNumProfiles|vaMaxNumEntrypoints|vaQueryConfigProfiles|vaQueryConfigEntrypoints"
FUNCS="$FUNCS|vaGetConfigAttributes|vaCreateConfig|vaDestroyConfig|vaQuerySurfaceAttributes"
FUNCS="$FUNCS|vaCreateContext|vaDestroyContext|vaCreateSurfaces|vaDestroySurfaces"
FUNCS="$FUNCS|vaExportSurfaceHandle|vaSyncSurface|vaSyncBuffer|vaQuerySurfaceStatus"
FUNCS="$FUNCS|vaCreateBuffer|vaDestroyBuffer|vaMapBuffer|vaUnmapBuffer"
FUNCS="$FUNCS|vaBeginPicture|vaRenderPicture|vaEndPicture"
FUNCS="$FUNCS|vaCreateImage|vaDestroyImage|vaGetImage|vaPutImage|vaDeriveImage"
FUNCS="$FUNCS|vaSetErrorCallback|vaSetInfoCallback"
TYPES='VADRMPRIMESurfaceDescriptor|VASurfaceAttrib|VASurfaceAttribExternalBuffers'
TYPES="$TYPES|VAEncSequenceParameterBufferH264|VAEncPictureParameterBufferH264|VAEncSliceParameterBufferH264"
TYPES="$TYPES|VAEncMiscParameterBuffer|VAEncMiscParameterRateControl|VAEncMiscParameterFrameRate|VAEncMiscParameterHRD"
TYPES="$TYPES|VAEncPackedHeaderParameterBuffer|VACodedBufferSegment"
TYPES="$TYPES|VAPictureParameterBufferH264|VAIQMatrixBufferH264|VASliceParameterBufferH264|VAPictureH264"
TYPES="$TYPES|VAImage|VAImageFormat|VAConfigAttrib|VAProfile|VAEntrypoint|VABufferType|VAStatus|VADisplay"
TYPES="$TYPES|VAEncPackedHeaderType|VASurfaceStatus|VAGenericValue|VAGenericValueType|VASurfaceAttribType"
bindgen wrapper.h \
    --allowlist-function "($FUNCS)" \
    --allowlist-type "_?($TYPES)" \
    --allowlist-var 'VA_(PROGRESSIVE|TIMEOUT_INFINITE|STATUS|RT_FORMAT|FOURCC|SURFACE_ATTRIB|EXPORT_SURFACE|RC|ENC_PACKED_HEADER|PICTURE_H264|INVALID|ATTRIB|PADDING|SLICE_TYPE|CODED_BUF|ENC_SLICE_TYPE|FRAME_PICTURE|TOP_FIELD|BOTTOM_FIELD)_?.*' \
    --no-layout-tests \
    --no-doc-comments \
    --no-prepend-enum-name \
    --raw-line '#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, unsafe_op_in_unsafe_fn, clippy::all, clippy::undocumented_unsafe_blocks)]' \
    -o ../src/bindings.rs \
    -- $(pkg-config --cflags libva)
rustfmt --edition 2021 ../src/bindings.rs
