// Checks the C++ operators of the C library from a C++ program that preloads it: a throwing new
// that cannot allocate runs the new-handler and throws std::bad_alloc, or what the handler throws,
// through the library, and a nothrow new runs the handler and returns null. Run by `just cabi-cxx`.
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <dlfcn.h>
#include <new>

static int failures;

static void check(bool ok, const char *what) {
  std::printf("%s: %s\n", ok ? "ok  " : "FAIL", what);
  failures += !ok;
}

// Too big for any allocator, and opaque to the compiler, so that no call is folded away.
static volatile std::size_t huge = SIZE_MAX / 2;
static void *volatile sink;

static int handler_calls;
struct HandlerGaveUp {};
static void giving_up_handler() {
  ++handler_calls;
  throw HandlerGaveUp{};
}

template <typename F> static bool throws_bad_alloc(F allocate) {
  try {
    allocate();
  } catch (const std::bad_alloc &) {
    return true;
  }
  return false;
}

template <typename F> static bool handler_ran_and_gave_up(F allocate) {
  handler_calls = 0;
  try {
    allocate();
  } catch (const HandlerGaveUp &) {
    return handler_calls == 1;
  }
  return false;
}

int main() {
  Dl_info info;
  void *symbol = dlsym(RTLD_DEFAULT, "_Znwm");
  check(symbol && dladdr(symbol, &info) && std::strstr(info.dli_fname, "bcmalloc"),
        "operator new is the library's");

  const std::align_val_t align{64};
  check(throws_bad_alloc([&] { sink = ::operator new(huge); }), "new throws bad_alloc");
  check(throws_bad_alloc([&] { sink = ::operator new[](huge); }), "new[] throws bad_alloc");
  check(throws_bad_alloc([&] { sink = ::operator new(huge, align); }),
        "aligned new throws bad_alloc");
  check(throws_bad_alloc([&] { sink = ::operator new[](huge, align); }),
        "aligned new[] throws bad_alloc");
  check(::operator new(huge, std::nothrow) == nullptr, "nothrow new returns null");
  check(::operator new[](huge, align, std::nothrow) == nullptr,
        "aligned nothrow new[] returns null");

  std::set_new_handler(giving_up_handler);
  check(handler_ran_and_gave_up([&] { sink = ::operator new(huge); }),
        "new runs the handler, whose exception reaches the caller");
  check(handler_ran_and_gave_up([&] { sink = ::operator new[](huge, align); }),
        "aligned new[] runs the handler, whose exception reaches the caller");
  handler_calls = 0;
  check(::operator new(huge, std::nothrow) == nullptr && handler_calls == 1,
        "nothrow new runs the handler and returns null");
  std::set_new_handler(nullptr);

  void *block = ::operator new(100);
  void *aligned = ::operator new[](100, std::align_val_t{256});
  check(block && aligned && reinterpret_cast<std::uintptr_t>(aligned) % 256 == 0,
        "new and aligned new[] allocate");
  ::operator delete(block, 100);
  ::operator delete[](aligned, std::align_val_t{256});

  return failures != 0;
}
