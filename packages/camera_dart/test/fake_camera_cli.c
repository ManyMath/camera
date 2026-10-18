// A stand-in for the camera_cli library, built by ffi_backend_test.dart.
//
// Implements the C ABI from native/camera_cli/src/lib.rs with two fake
// devices. Each started stream runs a thread that emits a tiny BGRA frame
// every few milliseconds. The fake_* functions let the test inject failures.

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef void (*frame_cb)(uint8_t *, int32_t, int32_t, int32_t, int32_t,
                         int64_t);

#define MAX_HANDLES 64

struct stream {
  pthread_t thread;
  atomic_int running;
  int active;
  frame_cb cb;
  int end_after;
};

static struct stream streams[MAX_HANDLES];
static int next_handle = 1;
static atomic_int starts;
static atomic_int fail_next_start;
static atomic_int end_after = -1;

// Test controls.
int32_t fake_start_count(void) { return atomic_load(&starts); }
void fake_fail_next_start(void) { atomic_store(&fail_next_start, 1); }
// Streams started from now on send the stream-ended signal after n frames
// (-1: never).
void fake_end_after(int32_t n) { atomic_store(&end_after, n); }

char *camera_cli_enumerate(void) {
  return strdup("[{\"id\":\"fake0\",\"name\":\"Fake\",\"facing\":\"external\","
                "\"formats\":[{\"width\":2,\"height\":2,"
                "\"pixel_format\":\"bgra8888\",\"frame_rate\":30}]},"
                "{\"id\":\"fake1\",\"name\":\"Front\",\"facing\":\"front\","
                "\"formats\":[]}]");
}

void camera_cli_string_free(char *s) { free(s); }

int32_t camera_cli_open(const char *id) {
  if (id == NULL || next_handle >= MAX_HANDLES) return -1;
  return next_handle++;
}

static void *run(void *arg) {
  struct stream *s = arg;
  int64_t ts = 0;
  int left = s->end_after;
  while (atomic_load(&s->running)) {
    if (left == 0) {
      s->cb(NULL, 0, 0, 0, -1, 0);
      return NULL;
    }
    if (left > 0) left--;
    uint8_t *data = malloc(16);
    memset(data, 0x7f, 16);
    s->cb(data, 16, 2, 2, 1, ts += 5000);
    usleep(5000);
  }
  return NULL;
}

static void stop(int32_t h) {
  if (h <= 0 || h >= MAX_HANDLES || !streams[h].active) return;
  atomic_store(&streams[h].running, 0);
  pthread_join(streams[h].thread, NULL);
  streams[h].active = 0;
}

int32_t camera_cli_start_stream(int32_t h, frame_cb cb) {
  if (h <= 0 || h >= MAX_HANDLES) return -3;
  atomic_fetch_add(&starts, 1);
  if (atomic_exchange(&fail_next_start, 0)) return -9;
  stop(h);
  streams[h].cb = cb;
  streams[h].end_after = atomic_load(&end_after);
  atomic_store(&streams[h].running, 1);
  streams[h].active = 1;
  pthread_create(&streams[h].thread, NULL, run, &streams[h]);
  return 0;
}

int32_t camera_cli_stop_stream(int32_t h) {
  stop(h);
  return 0;
}

void camera_cli_frame_free(uint8_t *data, int32_t length) {
  (void)length;
  free(data);
}

void camera_cli_close(int32_t h) { stop(h); }

void camera_cli_close_all(void) {
  for (int h = 1; h < MAX_HANDLES; h++) stop(h);
}
