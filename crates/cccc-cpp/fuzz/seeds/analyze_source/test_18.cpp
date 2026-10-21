
            class Stack {
            public:
                Stack() : top_(0) {}
                ~Stack() {}

                bool empty() const {
                    return top_ == 0;
                }

                void push(int x) {
                    if (top_ >= capacity_) {
                        grow();
                    }
                    data_[top_++] = x;
                }

                int pop() {
                    if (empty()) {
                        return -1;
                    }
                    return data_[--top_];
                }

            private:
                void grow() {
                    capacity_ *= 2;
                }

                int data_[1024];
                int top_;
                int capacity_ = 1024;
            };
        