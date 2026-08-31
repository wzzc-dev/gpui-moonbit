#include <stdint.h>

#if defined(__APPLE__)
#include <os/log.h>
#include <os/signpost.h>

static os_log_t benchmark_log(void) {
  static os_log_t log;
  if (log == NULL) {
    log = os_log_create("md_editor.benchmark", OS_LOG_CATEGORY_POINTS_OF_INTEREST);
  }
  return log;
}

void md_editor_benchmark_signpost_event(int32_t action_id) {
  os_log_t log = benchmark_log();
  if (log != NULL) {
    os_log(log, "md_editor_action id=%d", action_id);
    os_signpost_event_emit(log, OS_SIGNPOST_ID_EXCLUSIVE,
                           "md_editor_action", "id=%d", action_id);
  }
}
#else
void md_editor_benchmark_signpost_event(int32_t action_id) {
  (void)action_id;
}
#endif
