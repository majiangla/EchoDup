#pragma once

#include "ScanProgress.h"
#include <functional>

namespace EchoDup::Core
{
using ScanProgressCallback = std::function<void(const ScanProgress&)>;
}
