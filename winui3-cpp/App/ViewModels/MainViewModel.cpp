#include "MainViewModel.h"

namespace EchoDup::UI {

void MainViewModel::AddFile(const std::wstring& path)
{
    m_files.push_back(path);
}

const std::vector<std::wstring>& MainViewModel::Files() const
{
    return m_files;
}

}
