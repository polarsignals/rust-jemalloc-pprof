#ifndef JEMALLOC_PPROF_H
#define JEMALLOC_PPROF_H

#include <stddef.h>

#define JP_SUCCESS 0
#define JP_FAILURE 1

#ifdef __cplusplus
extern "C"
#endif
int dump_jemalloc_pprof(char **buf_out, size_t *n_out);

#endif // include guard
