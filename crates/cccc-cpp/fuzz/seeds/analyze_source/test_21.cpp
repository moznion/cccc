
            template <typename T>
            Queue<T>::~Queue() {
              if (this->data != nullptr) delete[] (this->data);
            }
            