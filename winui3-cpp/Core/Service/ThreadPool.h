#pragma once

#include <functional>

namespace EchoDup::Core
{
class ThreadPool
{
public:
    void Submit(std::function<void()> task);
};
}
