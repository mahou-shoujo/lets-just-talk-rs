class EncoderProcessor extends AudioWorkletProcessor {
  process(inputs, outputs, parameters) {
    if (inputs.length > 0) {
      const inputChannels = inputs[0];
      if (inputChannels && inputChannels.length > 0) {
        const audioFrame = new Float32Array(inputChannels[0]);
        if (!isZero(audioFrame)) {
          this.port.postMessage(audioFrame, { transfer: [audioFrame.buffer] });
        }
      }
    }
    return true;
  }
}

function isZero(array) {
  for (let index = 0; index < array.length; index++) {
    const element = array[index];
    if (element !== 0.0) {
      return false;
    }
  }
  return true;
}

registerProcessor("encoder-processor", EncoderProcessor);

class DecoderProcessor extends AudioWorkletProcessor {
  #ring;

  constructor() {
    super();
    this.port.onmessage = (event) => {
      const message = event.data;
      switch (message.type) {
        case "config":
          this.#ring = new Ring(message.sampleRate * message.sampleWindow);
          break;
        case "data":
          if (this.#ring) {
            this.#ring.enqueue(message.data);
          }
          break;
      }
    };
  }

  process(inputs, outputs, parameters) {
    if (outputs.length > 0) {
      const outputChannels = outputs[0];
      if (outputChannels && outputChannels.length > 0) {
        const monoChannel = outputChannels[0];
        if (this.#ring) {
          this.#ring.dequeue(monoChannel);
        } else {
          monoChannel.fill(0);
        }
        for (let i = 1; i < outputChannels.length; i++) {
          outputChannels[i].set(monoChannel);
        }
      }
    }
    return true;
  }
}

registerProcessor("decoder-processor", DecoderProcessor);

class Ring {
  buffer;
  write = 0;
  drain = 0;
  available = 0;

  constructor(capacity) {
    this.buffer = new Float32Array(capacity);
  }

  enqueue(input) {
    const capacity = this.buffer.length;
    if (input.length > capacity) {
      // Cut to capacity.
      input = input.subarray(input.length - capacity);
    }
    const len = input.length;
    const lenToEnd = Math.min(len, capacity - this.write);
    this.buffer.set(input.subarray(0, lenToEnd), this.write);
    if (lenToEnd < len) {
      this.buffer.set(input.subarray(lenToEnd), 0);
    }
    this.available += len;
    this.write = (this.write + len) % capacity;
    if (this.available > capacity) {
      // The drain index can't be behind the write index.
      this.available = capacity;
      this.drain = this.write;
    }
  }

  dequeue(output) {
    const capacity = this.buffer.length;
    const len = output.length;
    const readable = Math.min(len, this.available);
    const readableToEnd = Math.min(readable, capacity - this.drain);
    output.set(this.buffer.subarray(this.drain, this.drain + readableToEnd), 0);
    if (readableToEnd < readable) {
      output.set(
        this.buffer.subarray(0, readable - readableToEnd),
        readableToEnd,
      );
    }
    this.drain = (this.drain + readable) % this.buffer.length;
    this.available -= readable;
    if (readable < len) {
      output.fill(0, readable);
    }
  }
}
