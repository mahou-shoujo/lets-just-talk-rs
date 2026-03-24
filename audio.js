class EncoderProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.sampleRate = 0;
    this.port.onmessage = (event) => {
      const message = event.data;
      switch (message.type) {
        case "config":
          this.sampleRate = message.sampleRate;
          break;
      }
    };
  }

  process(inputs, outputs, parameters) {
    const inputChannels = inputs[0];
    if (inputChannels && inputChannels.length > 0) {
      const audioFrame = new Float32Array(inputChannels[0]);
      this.port.postMessage(audioFrame);
    }
    return true;
  }
}

registerProcessor("encoder-processor", EncoderProcessor);

class DecoderProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.sampleRate = 0;
    this.sampleWindow = 0;
    this.buffer;
    this.write = 0;
    this.drain = 0;
    this.available = 0;
    this.port.onmessage = (event) => {
      const message = event.data;
      switch (message.type) {
        case "config":
          this.sampleRate = message.sampleRate;
          this.sampleWindow = message.sampleWindow;
          this.buffer = new Float32Array(this.sampleRate * this.sampleWindow);
          this.write = 0;
          this.drain = 0;
          this.available = 0;
          break;
        case "data":
          this.enqueue(message.data);
          break;
      }
    };
  }

  enqueue(input) {
    if (!this.buffer) {
      return;
    }
    const len = input.length;
    if (len === 0) return;
    if (this.available + len > this.buffer.length) {
      // On overflow, drop the oldest samples to make room.
      const excess = this.available + len - this.buffer.length;
      const overflow = this.drain + excess;
      this.drain = overflow % this.buffer.length;
      this.available -= excess;
    }
    const lenToTheEnd = Math.min(len, this.buffer.length - this.write);
    this.buffer.set(input.subarray(0, lenToTheEnd), this.write);
    if (lenToTheEnd < len) {
      // Wrap around...
      this.buffer.set(input.subarray(lenToTheEnd), 0);
    }
    const overflow = this.write + len;
    this.write = overflow % this.buffer.length;
    this.available += len;
  }

  dequeue(output) {
    if (!this.buffer) {
      output.fill(0);
      return;
    }
    const len = output.length;
    const readable = Math.min(len, this.available);
    const readableUntilTheEnd = Math.min(
      readable,
      this.buffer.length - this.drain,
    );
    output.set(
      this.buffer.subarray(this.drain, this.drain + readableUntilTheEnd),
      0,
    );
    if (readableUntilTheEnd < readable) {
      // Wrap around...
      output.set(
        this.buffer.subarray(0, readable - readableUntilTheEnd),
        readableUntilTheEnd,
      );
    }
    const overflow = this.drain + readable;
    this.drain = overflow % this.buffer.length;
    this.available -= readable;
    if (readable < len) {
      output.fill(0, readable);
    }
  }

  process(inputs, outputs, parameters) {
    const outputChannels = outputs[0];
    if (outputChannels && outputChannels.length > 0) {
      const monoChannel = outputChannels[0];
      this.dequeue(monoChannel);
      for (let i = 1; i < outputChannels.length; i++) {
        outputChannels[i].set(monoChannel);
      }
    }
    return true;
  }
}

registerProcessor("decoder-processor", DecoderProcessor);
